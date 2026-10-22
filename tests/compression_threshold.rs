//! Messages below `compression_threshold` go out without RSV1, and mixing them with
//! compressed messages keeps the peer's context in sync.
use futures::{SinkExt, StreamExt};
use tokio::io::{duplex, AsyncReadExt};
use yawc::{Frame, Options, Role, WebSocket};

const EXTENSIONS: Option<&str> = Some("permessage-deflate");
const THRESHOLD: usize = 64;
/// FIN plus the text opcode, the first header byte of an unfragmented text frame.
const FIN_TEXT: u8 = 0x81;
const RSV1: u8 = 0x40;

fn options() -> Options {
    Options::default()
        .with_low_latency_compression()
        .with_compression_threshold(THRESHOLD)
}

#[tokio::test]
async fn small_messages_are_sent_uncompressed() {
    let (io, mut peer) = duplex(64 * 1024);
    let mut server =
        WebSocket::from_stream_with_extensions(io, Role::Server, EXTENSIONS, options())
            .expect("the server accepts permessage-deflate");

    let small = "x".repeat(THRESHOLD - 1);
    server.send(Frame::text(small.clone())).await.unwrap();
    // A server frame is unmasked: two header bytes, then the payload as is.
    let mut wire = vec![0; 2 + small.len()];
    peer.read_exact(&mut wire).await.unwrap();
    assert_eq!(wire[0], FIN_TEXT, "a small message must not set RSV1");
    assert_eq!(&wire[2..], small.as_bytes());

    server
        .send(Frame::text("x".repeat(THRESHOLD)))
        .await
        .unwrap();
    let mut header = [0; 1];
    peer.read_exact(&mut header).await.unwrap();
    assert_eq!(
        header[0],
        FIN_TEXT | RSV1,
        "a message at the threshold is compressed"
    );
}

#[tokio::test]
async fn mixed_sizes_round_trip_with_context_takeover() {
    let (server_io, client_io) = duplex(64 * 1024);
    let mut server =
        WebSocket::from_stream_with_extensions(server_io, Role::Server, EXTENSIONS, options())
            .expect("the server accepts permessage-deflate");
    let mut client =
        WebSocket::from_stream_with_extensions(client_io, Role::Client, EXTENSIONS, options())
            .expect("the client accepts permessage-deflate");

    let large = r#"["EVENT","subscription-1",{"kind":1,"content":"hello"}]"#.repeat(4);
    let messages = ["OK", large.as_str(), "EOSE", large.as_str(), "OK"];
    for message in messages {
        server.send(Frame::text(message.to_owned())).await.unwrap();
        let received = client.next().await.expect("client receives");
        assert_eq!(received.as_str(), message);

        client.send(Frame::text(message.to_owned())).await.unwrap();
        let received = server.next().await.expect("server receives");
        assert_eq!(received.as_str(), message);
    }
}
