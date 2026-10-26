//! The server's answer to a client's permessage-deflate offers.
//!
//! A client may offer permessage-deflate several times, most preferred first, list other
//! extensions beside it, quote parameter values and split the list across header lines
//! (RFC 6455, Section 9.1; RFC 7692, Section 5). These tests send such handshakes to both
//! server entry points, the hyper upgrade and the axum extractor, and check the
//! `Sec-WebSocket-Extensions` answer. One of them then exchanges compressed messages over
//! the connection it negotiated.

#![cfg(not(target_arch = "wasm32"))]

use std::net::SocketAddr;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use http_body_util::Empty;
use hyper::{
    body::Incoming, header, server::conn::http1, service::service_fn, Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use yawc::{Frame, OpCode, Options, Role, WebSocket};

/// The example key from RFC 6455, Section 1.3.
const SAMPLE_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// Autobahn's case 13.7 offers these three, most preferred first.
const AUTOBAHN_13_7: &str = "permessage-deflate; client_no_context_takeover; \
    client_max_window_bits; server_no_context_takeover; server_max_window_bits=9, \
    permessage-deflate; client_no_context_takeover; client_max_window_bits; \
    server_no_context_takeover, \
    permessage-deflate; client_no_context_takeover; client_max_window_bits";

/// The answer to [`AUTOBAHN_13_7`]. zlib takes the first offer. The default backend always
/// compresses with the full 32 KiB window, cannot keep to the 9-bit one the first offer
/// asks of the server, and takes the second.
#[cfg(feature = "zlib")]
const ANSWER_13_7: &str = "permessage-deflate; server_max_window_bits=9; \
    client_max_window_bits=9; server_no_context_takeover; client_no_context_takeover";
#[cfg(not(feature = "zlib"))]
const ANSWER_13_7: &str =
    "permessage-deflate; server_no_context_takeover; client_no_context_takeover";

fn options() -> Options {
    Options::default().with_low_latency_compression()
}

/// Echoes text and binary frames until the client goes away.
async fn echo(upgrade: yawc::UpgradeFut) {
    let Ok(mut ws) = upgrade.await else { return };
    while let Some(frame) = ws.next().await {
        if matches!(frame.opcode(), OpCode::Text | OpCode::Binary) && ws.send(frame).await.is_err()
        {
            break;
        }
    }
}

/// A server that upgrades with [`WebSocket::upgrade_with_options`].
async fn spawn_hyper_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let service = service_fn(|mut request: Request<Incoming>| async move {
                let (response, upgrade) = WebSocket::upgrade_with_options(&mut request, options())?;
                tokio::spawn(echo(upgrade));
                yawc::Result::Ok(response)
            });
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });

    addr
}

/// Sends an upgrade request carrying `lines` as its `Sec-WebSocket-Extensions` header
/// lines, and returns the `101` response.
async fn handshake(addr: SocketAddr, lines: &[&str]) -> Response<Incoming> {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn.with_upgrades());

    let mut request = Request::builder()
        .method("GET")
        .uri("/")
        .header(header::HOST, "localhost")
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "upgrade")
        .header(header::SEC_WEBSOCKET_KEY, SAMPLE_KEY)
        .header(header::SEC_WEBSOCKET_VERSION, "13");
    for line in lines {
        request = request.header(header::SEC_WEBSOCKET_EXTENSIONS, *line);
    }

    let response = sender
        .send_request(request.body(Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    response
}

/// The server's `Sec-WebSocket-Extensions` answer, if it sent one.
fn answer(response: &Response<Incoming>) -> Option<&str> {
    response
        .headers()
        .get(header::SEC_WEBSOCKET_EXTENSIONS)
        .map(|value| value.to_str().unwrap())
}

#[tokio::test]
async fn answers_the_first_offer_it_can_honor() {
    let addr = spawn_hyper_server().await;
    let response = handshake(addr, &[AUTOBAHN_13_7]).await;
    assert_eq!(answer(&response), Some(ANSWER_13_7));
}

#[tokio::test]
async fn reads_offers_across_header_lines_and_quoted_values() {
    let addr = spawn_hyper_server().await;
    let response = handshake(
        addr,
        &[
            "x-webkit-deflate-frame",
            r#"permessage-deflate; client_max_window_bits="12""#,
        ],
    )
    .await;

    // Only zlib can hold the client to a smaller window.
    let expected = if cfg!(feature = "zlib") {
        "permessage-deflate; client_max_window_bits=12"
    } else {
        "permessage-deflate"
    };
    assert_eq!(answer(&response), Some(expected));
}

#[tokio::test]
async fn declines_offers_it_cannot_honor() {
    let addr = spawn_hyper_server().await;
    // A parameter RFC 7692 does not define, and a server window neither backend's
    // compressor can keep to.
    let response = handshake(
        addr,
        &["permessage-deflate; foo=1, permessage-deflate; server_max_window_bits=8"],
    )
    .await;
    assert_eq!(answer(&response), None);
}

#[tokio::test]
async fn compresses_both_ways_with_the_offer_it_chose() {
    let addr = spawn_hyper_server().await;
    let response = handshake(addr, &[AUTOBAHN_13_7]).await;
    let extensions = answer(&response).map(str::to_owned);
    assert!(extensions.is_some());

    let upgraded = hyper::upgrade::on(response).await.unwrap();
    let mut client = WebSocket::from_stream_with_extensions(
        TokioIo::new(upgraded),
        Role::Client,
        extensions.as_deref(),
        options(),
    )
    .unwrap();

    // Several messages, since neither side keeps its window from one to the next.
    let payload = "compress me ".repeat(4096);
    for _ in 0..3 {
        client.send(Frame::text(payload.clone())).await.unwrap();
        let frame = client.next().await.unwrap();
        assert_eq!(frame.payload().as_ref(), payload.as_bytes());
    }
}

#[cfg(feature = "axum")]
mod axum_extractor {
    use axum::{response::IntoResponse, routing::get, Router};
    use hyper_util::service::TowerToHyperService;
    use yawc::IncomingUpgrade;

    use super::*;

    async fn handler(ws: IncomingUpgrade) -> impl IntoResponse {
        let (response, upgrade) = ws.upgrade(options()).unwrap();
        tokio::spawn(echo(upgrade));
        response
    }

    async fn spawn_server() -> SocketAddr {
        let app = Router::new().route("/", get(handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let service = TowerToHyperService::new(app.clone());
                tokio::spawn(async move {
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });

        addr
    }

    #[tokio::test]
    async fn answers_the_first_offer_it_can_honor() {
        let addr = spawn_server().await;
        let response = handshake(addr, &[AUTOBAHN_13_7]).await;
        assert_eq!(answer(&response), Some(ANSWER_13_7));
    }

    #[tokio::test]
    async fn reads_offers_across_header_lines() {
        let addr = spawn_server().await;
        let response = handshake(addr, &["x-webkit-deflate-frame", "permessage-deflate"]).await;
        assert_eq!(answer(&response), Some("permessage-deflate"));
    }
}
