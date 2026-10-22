//! Buffers far smaller than a message still carry it, compressed or not.
use futures::{SinkExt, StreamExt};
use tokio::io::duplex;
use yawc::{Frame, Options, Role, WebSocket};

async fn round_trip(extensions: Option<&str>) {
    let options = Options::default()
        .with_low_latency_compression()
        .with_read_buffer_capacity(64)
        .with_write_buffer_capacity(64);
    let (server_io, client_io) = duplex(16 * 1024);
    let mut server = WebSocket::from_stream_with_extensions(
        server_io,
        Role::Server,
        extensions,
        options.clone(),
    )
    .expect("the server accepts the extensions");
    let mut client =
        WebSocket::from_stream_with_extensions(client_io, Role::Client, extensions, options)
            .expect("the client accepts the extensions");

    let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    let sender = tokio::spawn(async move {
        client.send(Frame::binary(payload)).await.unwrap();
        client
    });
    let received = server.next().await.expect("server receives");
    let expected: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    assert_eq!(received.payload().as_ref(), expected.as_slice());

    let mut client = sender.await.unwrap();
    server.send(Frame::text("ok")).await.unwrap();
    assert_eq!(client.next().await.expect("client receives").as_str(), "ok");
}

#[tokio::test]
async fn large_message_through_small_buffers() {
    round_trip(None).await;
}

#[tokio::test]
async fn large_compressed_message_through_small_buffers() {
    round_trip(Some("permessage-deflate")).await;
}
