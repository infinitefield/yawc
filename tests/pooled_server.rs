//! A server that forces no context takeover in both directions, so every connection
//! can share a `DeflatePool`, works with a client that offered neither flag.
use std::{convert::Infallible, net::SocketAddr};

use futures::{SinkExt, StreamExt};
use hyper::{
    body::Incoming, header, server::conn::http1, service::service_fn, Request, StatusCode,
};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use yawc::{DeflatePool, Frame, OpCode, Options, Role, WebSocket};

/// What a browser sends: deflate, with no context-takeover flags.
const BROWSER_OFFER: &str = "permessage-deflate; client_max_window_bits";

fn server_options(pool: &DeflatePool) -> Options {
    Options::default()
        .with_low_latency_compression()
        .server_no_context_takeover()
        .client_no_context_takeover()
        .with_deflate_pool(pool.clone())
}

async fn spawn_server(pool: DeflatePool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let pool = pool.clone();
            let service = service_fn(move |mut request: Request<Incoming>| {
                let options = server_options(&pool);
                async move {
                    let (response, upgrade) =
                        WebSocket::upgrade_with_options(&mut request, options).unwrap();
                    tokio::spawn(async move {
                        let mut ws = upgrade.await.unwrap();
                        while let Some(frame) = ws.next().await {
                            if frame.opcode() == OpCode::Text && ws.send(frame).await.is_err() {
                                break;
                            }
                        }
                    });
                    Ok::<_, Infallible>(response)
                }
            });
            tokio::spawn(
                http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades(),
            );
        }
    });
    addr
}

#[tokio::test]
async fn browser_offer_gets_no_context_takeover_both_ways() {
    let addr = spawn_server(DeflatePool::new(8)).await;
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn.with_upgrades());

    let request = Request::builder()
        .uri("/")
        .header(header::HOST, "localhost")
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "upgrade")
        .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(header::SEC_WEBSOCKET_EXTENSIONS, BROWSER_OFFER)
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

    let agreed = response
        .headers()
        .get(header::SEC_WEBSOCKET_EXTENSIONS)
        .expect("the server agrees to permessage-deflate")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(agreed.contains("server_no_context_takeover"), "{agreed}");
    assert!(agreed.contains("client_no_context_takeover"), "{agreed}");

    let upgraded = hyper::upgrade::on(response).await.unwrap();
    let mut client = WebSocket::from_stream_with_extensions(
        TokioIo::new(upgraded),
        Role::Client,
        Some(&agreed),
        Options::default().with_low_latency_compression(),
    )
    .expect("the client accepts what the server agreed to");

    for message in ["hello", "hello again", r#"["EOSE","subscription-1"]"#] {
        client.send(Frame::text(message)).await.unwrap();
        let echo = client.next().await.expect("the server echoes");
        assert_eq!(echo.as_str(), message);
    }
}
