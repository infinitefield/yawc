//! Tests for wrapping a stream that was handshaked elsewhere.
//!
//! These cover [`WebSocket::from_stream`] and
//! [`WebSocket::from_stream_with_extensions`], which perform no handshake and take the
//! caller's word for the role and negotiated extensions.

// The wasm build compiles integration tests too, and neither this API nor tokio's duplex
// exists there.
#![cfg(not(target_arch = "wasm32"))]

use futures::{SinkExt, StreamExt};
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};
use yawc::{frame::OpCode, Frame, Options, Role, WebSocket, WebSocketError};

/// Builds a connected client/server pair over an in-memory duplex.
fn pair(options: Options) -> (WebSocket<DuplexStream>, WebSocket<DuplexStream>) {
    let (client_io, server_io) = duplex(1024 * 1024);

    let client = WebSocket::from_stream(client_io, Role::Client, options.clone()).unwrap();
    let server = WebSocket::from_stream(server_io, Role::Server, options).unwrap();

    (client, server)
}

#[tokio::test]
async fn round_trips_in_both_directions() {
    let (mut client, mut server) = pair(Options::default());

    client.send(Frame::text("ping from client")).await.unwrap();
    let frame = server.next().await.unwrap();
    assert_eq!(frame.opcode(), OpCode::Text);
    assert_eq!(frame.payload().as_ref(), b"ping from client");

    server.send(Frame::binary(vec![9, 8, 7])).await.unwrap();
    let frame = client.next().await.unwrap();
    assert_eq!(frame.opcode(), OpCode::Binary);
    assert_eq!(frame.payload().as_ref(), &[9, 8, 7]);
}

#[tokio::test]
async fn role_client_masks_and_role_server_does_not() {
    // Read the raw bytes off the wire rather than through a peer WebSocket, so the mask
    // bit is observed directly. Getting the role wrong is the main way to misuse this
    // API, so it is worth pinning down.
    let (client_io, mut raw) = duplex(1024);
    let mut client = WebSocket::from_stream(client_io, Role::Client, Options::default()).unwrap();
    client.send(Frame::text("hi")).await.unwrap();

    let mut header = [0u8; 2];
    raw.read_exact(&mut header).await.unwrap();
    assert_eq!(header[0], 0x81, "expected FIN + text opcode");
    assert_eq!(header[1] & 0x80, 0x80, "client frames must be masked");
    assert_eq!(header[1] & 0x7f, 2, "payload length");

    let (server_io, mut raw) = duplex(1024);
    let mut server = WebSocket::from_stream(server_io, Role::Server, Options::default()).unwrap();
    server.send(Frame::text("hi")).await.unwrap();

    let mut header = [0u8; 2];
    raw.read_exact(&mut header).await.unwrap();
    assert_eq!(header[0], 0x81);
    assert_eq!(header[1] & 0x80, 0x00, "server frames must not be masked");
}

#[tokio::test]
async fn honors_negotiated_permessage_deflate() {
    let (client_io, server_io) = duplex(1024 * 1024);
    let options = Options::default().with_balanced_compression();

    let mut client = WebSocket::from_stream_with_extensions(
        client_io,
        Role::Client,
        Some("permessage-deflate"),
        options.clone(),
    )
    .unwrap();

    let mut server = WebSocket::from_stream_with_extensions(
        server_io,
        Role::Server,
        Some("permessage-deflate"),
        options,
    )
    .unwrap();

    let payload = "compress me ".repeat(4096);
    client.send(Frame::text(payload.clone())).await.unwrap();

    let frame = server.next().await.unwrap();
    assert_eq!(frame.payload().as_ref(), payload.as_bytes());
}

#[tokio::test]
async fn compression_stays_off_when_not_negotiated() {
    // Compression is enabled locally but the handshake agreed on nothing, so frames must
    // go out uncompressed. A peer with no inflate context has to be able to read them.
    let (client_io, server_io) = duplex(1024 * 1024);
    let with_compression = Options::default().with_balanced_compression();

    let mut client =
        WebSocket::from_stream(client_io, Role::Client, with_compression.clone()).unwrap();
    let mut server = WebSocket::from_stream(server_io, Role::Server, Options::default()).unwrap();

    let payload = "plain ".repeat(1024);
    client.send(Frame::text(payload.clone())).await.unwrap();

    let frame = server.next().await.unwrap();
    assert_eq!(frame.payload().as_ref(), payload.as_bytes());
}

#[tokio::test]
async fn rejects_extensions_that_were_never_offered() {
    // The peer agreed on permessage-deflate but this side has no compression configured,
    // so there is no way to decode what arrives. Better to fail here than at read time.
    let (io, _peer) = duplex(1024);

    let result = WebSocket::from_stream_with_extensions(
        io,
        Role::Client,
        Some("permessage-deflate"),
        Options::default().without_compression(),
    );

    let err = match result {
        Ok(_) => panic!("expected the unoffered extension to be rejected"),
        Err(err) => err,
    };

    assert!(
        matches!(err, WebSocketError::CompressionNotSupported),
        "unexpected error: {err:?}"
    );
}

#[tokio::test]
async fn ignores_an_unparseable_extensions_header() {
    // Garbage in the header is treated as "nothing negotiated" rather than an error, so a
    // caller passing through an odd header still gets a working connection.
    let (client_io, server_io) = duplex(1024);

    let mut client = WebSocket::from_stream_with_extensions(
        client_io,
        Role::Client,
        Some("!!! not an extension !!!"),
        Options::default(),
    )
    .unwrap();
    let mut server = WebSocket::from_stream(server_io, Role::Server, Options::default()).unwrap();

    client.send(Frame::text("still works")).await.unwrap();
    assert_eq!(
        server.next().await.unwrap().payload().as_ref(),
        b"still works"
    );
}

#[tokio::test]
async fn control_frames_work_over_a_wrapped_stream() {
    let (mut client, mut server) = pair(Options::default());

    client
        .send(Frame::ping(&b"are you there"[..]))
        .await
        .unwrap();

    let frame = server.next().await.unwrap();
    assert_eq!(frame.opcode(), OpCode::Ping);

    // The pong is queued as an obligated send and is only written when the server is
    // polled again, so the server has to be driven alongside the client's read. Awaiting
    // the client alone deadlocks. `biased` polls the server first so the pong is flushed
    // before the client is checked.
    let frame = tokio::select! {
        biased;
        unexpected = server.next() => {
            panic!("server yielded an extra {:?} frame", unexpected.map(|f| f.opcode()))
        }
        frame = client.next() => frame.unwrap(),
    };

    assert_eq!(frame.opcode(), OpCode::Pong);
    assert_eq!(frame.payload().as_ref(), b"are you there");
}

#[tokio::test]
async fn reads_a_frame_written_by_hand() {
    // Nothing in this path came from a yawc peer, which is the point: the stream is
    // opaque and only has to carry well-formed RFC 6455 frames.
    let (server_io, mut raw) = duplex(1024);
    let mut server = WebSocket::from_stream(server_io, Role::Server, Options::default()).unwrap();

    // FIN + text, masked, 5 bytes, mask key 0x01020304, "hello" masked with it.
    let mask = [0x01, 0x02, 0x03, 0x04];
    let mut frame = vec![0x81, 0x85];
    frame.extend_from_slice(&mask);
    frame.extend(b"hello".iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));

    raw.write_all(&frame).await.unwrap();

    let received = server.next().await.unwrap();
    assert_eq!(received.opcode(), OpCode::Text);
    assert_eq!(received.payload().as_ref(), b"hello");
}
