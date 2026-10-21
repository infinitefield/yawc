#![cfg(not(target_arch = "wasm32"))]

use std::task::{Context, Poll};

use futures::SinkExt;
use tokio::io::{duplex, DuplexStream};
use yawc::{close::CloseCode, Frame, OpCode, Options, Role, WebSocket, WebSocketError};

fn pair(
    options: Options,
    receiver_role: Role,
    extensions: &str,
) -> (WebSocket<DuplexStream>, WebSocket<DuplexStream>) {
    let (sender, receiver) = duplex(128 * 1024);
    let sender_role = match receiver_role {
        Role::Client => Role::Server,
        Role::Server => Role::Client,
    };
    let sender = WebSocket::from_stream_with_extensions(
        sender,
        sender_role,
        Some(extensions),
        Options::default().with_balanced_compression(),
    )
    .unwrap();
    let receiver = WebSocket::from_stream_with_extensions(
        receiver,
        receiver_role,
        Some(extensions),
        options.with_balanced_compression(),
    )
    .unwrap();
    (sender, receiver)
}

#[tokio::test]
async fn oversized_compressed_frame_is_rejected() {
    let limit = 64 * 1024;
    let (mut sender, mut receiver) = pair(
        Options::default().with_max_payload_read(limit),
        Role::Server,
        "permessage-deflate",
    );
    sender
        .send(Frame::binary(vec![b'a'; 32 * 1024 * 1024]))
        .await
        .unwrap();
    assert!(matches!(
        receiver.next_frame().await,
        Err(WebSocketError::FrameTooLarge)
    ));
    assert!(matches!(
        receiver.next_frame().await,
        Err(WebSocketError::ConnectionClosed)
    ));
    let close = sender.next_frame().await.unwrap();
    assert_eq!(close.opcode(), OpCode::Close);
    assert_eq!(close.close_code(), Some(CloseCode::Size));
}

#[tokio::test]
async fn compressed_first_fragment_respects_reassembly_limit() {
    let (sender, receiver) = pair(
        Options::default().with_limits(64 * 1024, 1024),
        Role::Server,
        "permessage-deflate",
    );
    let mut sender = sender.into_streaming();
    let mut receiver = receiver.into_streaming();
    sender
        .send(Frame::binary(vec![b'a'; 16 * 1024]).with_fin(false))
        .await
        .unwrap();
    sender.send(Frame::continuation("")).await.unwrap();
    // Streaming has no reassembly buffer and can return each bounded fragment.
    let first = receiver.next_frame().await.unwrap();
    let last = receiver.next_frame().await.unwrap();
    assert_eq!(first.payload().len() + last.payload().len(), 16 * 1024);

    let (sender, mut receiver) = pair(
        Options::default().with_limits(64 * 1024, 1024),
        Role::Server,
        "permessage-deflate",
    );
    let mut sender = sender.into_streaming();
    sender
        .send(Frame::binary(vec![b'a'; 16 * 1024]).with_fin(false))
        .await
        .unwrap();
    // No final fragment is needed to reject an already oversized message.
    let waker = futures::task::noop_waker();
    assert!(matches!(
        receiver.poll_next_frame(&mut Context::from_waker(&waker)),
        Poll::Ready(Err(WebSocketError::FrameTooLarge))
    ));
}

#[tokio::test]
async fn compressed_and_uncompressed_frames_share_the_payload_limit() {
    let limit = 4096;
    for role in [Role::Client, Role::Server] {
        for extensions in [
            "",
            "permessage-deflate",
            "permessage-deflate; client_no_context_takeover; server_no_context_takeover",
        ] {
            for size in [0, limit - 1, limit, limit + 1] {
                let (mut sender, mut receiver) = pair(
                    Options::default().with_max_payload_read(limit),
                    role,
                    extensions,
                );
                sender.send(Frame::text("a".repeat(size))).await.unwrap();
                let result = receiver.next_frame().await;
                if size < limit {
                    assert_eq!(result.unwrap().payload().len(), size);
                } else {
                    assert!(matches!(result, Err(WebSocketError::FrameTooLarge)));
                }
            }
        }
    }
}

#[tokio::test]
async fn compressed_continuation_uses_remaining_reassembly_budget() {
    for size in [1999, 2000, 2001] {
        let (sender, mut receiver) = pair(
            Options::default().with_limits(2048, 2000),
            Role::Server,
            "permessage-deflate",
        );
        let mut sender = sender.into_streaming();
        sender
            .feed(Frame::binary(vec![b'a'; 1500]).with_fin(false))
            .await
            .unwrap();
        sender.feed(Frame::ping("ping")).await.unwrap();
        sender
            .feed(Frame::continuation(vec![b'a'; size - 1500]))
            .await
            .unwrap();
        sender.flush().await.unwrap();
        assert_eq!(receiver.next_frame().await.unwrap().opcode(), OpCode::Ping);
        let result = receiver.next_frame().await;
        if size < 2000 {
            assert_eq!(result.unwrap().payload().len(), size);
        } else {
            assert!(matches!(result, Err(WebSocketError::FrameTooLarge)));
        }
    }
}

#[tokio::test]
async fn compression_budget_resets_between_messages() {
    for extensions in [
        "permessage-deflate",
        "permessage-deflate; client_no_context_takeover; server_no_context_takeover",
    ] {
        let (sender, mut receiver) = pair(
            Options::default().with_limits(1024, 2000),
            Role::Server,
            extensions,
        );
        let mut sender = sender.into_streaming();
        for _ in 0..3 {
            sender
                .feed(Frame::binary(vec![b'a'; 600]).with_fin(false))
                .await
                .unwrap();
            sender
                .send(Frame::continuation(vec![b'a'; 600]))
                .await
                .unwrap();
            assert_eq!(receiver.next_frame().await.unwrap().payload().len(), 1200);
            sender.send(Frame::text("b".repeat(1023))).await.unwrap();
            assert_eq!(receiver.next_frame().await.unwrap().payload().len(), 1023);
        }
    }
}
