#![cfg(not(target_arch = "wasm32"))]

use std::{future::poll_fn, time::Duration};

use bytes::Bytes;
use futures::{stream::SplitSink, FutureExt, SinkExt, StreamExt};
use tokio::{io::duplex, time::timeout};
use yawc::{Frame, OpCode, Options, Role, WebSocket};

#[tokio::test]
async fn feed_applies_backpressure_before_accepting_another_message() {
    for fragmented in [false, true] {
        let (client_io, server_io) = duplex(1);
        let mut options = Options::default().with_backpressure_boundary(1);
        if fragmented {
            options = options.with_max_fragment_size(17);
        }
        let mut client = WebSocket::from_stream(client_io, Role::Client, options).unwrap();
        let mut server =
            WebSocket::from_stream(server_io, Role::Server, Options::default()).unwrap();
        client.feed(Frame::binary(vec![42; 257])).await.unwrap();
        assert!(poll_fn(|cx| client.poll_ready_unpin(cx))
            .now_or_never()
            .is_none());
        timeout(Duration::from_secs(5), async {
            let (sent, received) = tokio::join!(client.flush(), server.next_frame());
            sent.unwrap();
            assert_eq!(received.unwrap().payload().as_ref(), &[42; 257]);
        })
        .await
        .expect("draining the peer must release backpressure");
    }
}

#[tokio::test]
async fn feed_preserves_order_under_backpressure() {
    for fragment_size in [None, Some(17)] {
        let (client_io, server_io) = duplex(64);
        let mut options = Options::default().with_backpressure_boundary(32);
        if let Some(size) = fragment_size {
            options = options.with_max_fragment_size(size);
        }
        let mut client = WebSocket::from_stream(client_io, Role::Client, options).unwrap();
        let mut server =
            WebSocket::from_stream(server_io, Role::Server, Options::default()).unwrap();
        timeout(Duration::from_secs(5), async {
            tokio::join!(
                async {
                    for value in 0..32 {
                        client.feed(Frame::binary(vec![value; 257])).await.unwrap();
                    }
                    client.flush().await.unwrap();
                },
                async {
                    for value in 0..32 {
                        let frame = server.next_frame().await.unwrap();
                        assert_eq!(frame.opcode(), OpCode::Binary);
                        assert_eq!(frame.payload().as_ref(), &[value; 257]);
                    }
                },
            );
        })
        .await
        .expect("queued frames must make progress when the peer reads");
    }
}

#[tokio::test]
async fn split_reader_and_writer_wake_independent_tasks() {
    let (client_io, server_io) = duplex(64);
    let options = Options::default().with_backpressure_boundary(32);
    let client = WebSocket::from_stream(client_io, Role::Client, options.clone()).unwrap();
    let server = WebSocket::from_stream(server_io, Role::Server, options).unwrap();
    let (client_tx, mut client_rx) = client.split();
    let (server_tx, mut server_rx) = server.split();
    let send = |mut sink: SplitSink<WebSocket<_>, Frame>, value| async move {
        for _ in 0..32 {
            sink.send(Frame::binary(vec![value; 1024])).await.unwrap();
        }
    };
    let client_writer = tokio::spawn(send(client_tx, 7));
    let server_writer = tokio::spawn(send(server_tx, 9));
    timeout(Duration::from_secs(5), async {
        tokio::join!(
            async {
                for _ in 0..32 {
                    assert_eq!(
                        client_rx.next().await.unwrap().payload().as_ref(),
                        &[9; 1024]
                    );
                }
            },
            async {
                for _ in 0..32 {
                    assert_eq!(
                        server_rx.next().await.unwrap().payload().as_ref(),
                        &[7; 1024]
                    );
                }
            },
        );
        client_writer.await.unwrap();
        server_writer.await.unwrap();
    })
    .await
    .expect("reads and writes must wake each other across split tasks");
}

#[tokio::test]
async fn manual_fragments_and_ping_keep_their_order() {
    let (client_io, server_io) = duplex(64);
    let mut client = WebSocket::from_stream(client_io, Role::Client, Options::default()).unwrap();
    let mut server = WebSocket::from_stream(server_io, Role::Server, Options::default()).unwrap();
    client
        .feed(Frame::text("first ").with_fin(false))
        .await
        .unwrap();
    client
        .feed(Frame::ping(Bytes::from_static(b"ping")))
        .await
        .unwrap();
    client.feed(Frame::continuation("last")).await.unwrap();
    client.flush().await.unwrap();
    assert_eq!(server.next_frame().await.unwrap().opcode(), OpCode::Ping);
    let frame = server.next_frame().await.unwrap();
    assert_eq!(frame.opcode(), OpCode::Text);
    assert_eq!(frame.payload().as_ref(), b"first last");
}
