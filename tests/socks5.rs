//! End-to-end tests for dialling through a SOCKS5 proxy.
//!
//! The proxy is a mock that speaks the server half of RFC 1928 and then relays bytes, so
//! the tests cover what actually goes over the wire: which address type the request
//! carries, whether credentials are sent, and what a refusal turns into.

// The wasm build compiles integration tests too, and none of this exists there.
#![cfg(not(target_arch = "wasm32"))]

use std::{convert::Infallible, net::SocketAddr};

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use http_body_util::Empty;
use hyper::{body::Incoming, server::conn::http1, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use yawc::{
    frame::OpCode, Frame, Options, Proxy, ReplyCode, Socks5Error, WebSocket, WebSocketError,
};

/// What the mock proxy was asked to connect to.
#[derive(Debug, PartialEq, Eq)]
enum Requested {
    Domain { host: String, port: u16 },
    Addr(SocketAddr),
}

/// Credentials the mock proxy demands.
struct Credentials {
    username: String,
    password: String,
}

/// How the mock proxy behaves for one connection.
#[derive(Default)]
struct MockProxy {
    /// When set, the proxy offers only username/password and checks what it is given.
    credentials: Option<Credentials>,
    /// The reply code to answer the `CONNECT` with. Anything but 0 skips the relay.
    reply: u8,
}

impl MockProxy {
    /// Starts the proxy and returns its address and a channel of what it was asked for.
    async fn spawn(self) -> (SocketAddr, mpsc::UnboundedReceiver<Requested>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                self.negotiate(&mut client, &tx).await;
            }
        });

        (addr, rx)
    }

    /// Runs the server half of the handshake, reports what was asked for, then relays
    /// until either side closes.
    async fn negotiate(&self, client: &mut TcpStream, requests: &mpsc::UnboundedSender<Requested>) {
        let mut greeting = [0; 2];
        client.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting[0], 5);

        let mut methods = vec![0; usize::from(greeting[1])];
        client.read_exact(&mut methods).await.unwrap();

        match &self.credentials {
            Some(expected) => {
                assert!(methods.contains(&2), "client never offered a password");
                client.write_all(&[5, 2]).await.unwrap();

                let mut version = [0; 1];
                client.read_exact(&mut version).await.unwrap();
                assert_eq!(version[0], 1);

                let username = read_prefixed(client).await;
                let password = read_prefixed(client).await;

                let ok = username == expected.username && password == expected.password;
                client.write_all(&[1, u8::from(!ok)]).await.unwrap();
                if !ok {
                    return;
                }
            }
            None => {
                assert!(methods.contains(&0));
                client.write_all(&[5, 0]).await.unwrap();
            }
        }

        let mut head = [0; 4];
        client.read_exact(&mut head).await.unwrap();
        assert_eq!(&head[..3], [5, 1, 0]);

        let requested = match head[3] {
            1 => {
                let mut rest = [0; 6];
                client.read_exact(&mut rest).await.unwrap();
                let ip = [rest[0], rest[1], rest[2], rest[3]];
                let port = u16::from_be_bytes([rest[4], rest[5]]);
                Requested::Addr(SocketAddr::from((ip, port)))
            }
            3 => {
                let host = read_prefixed(client).await;
                let mut port = [0; 2];
                client.read_exact(&mut port).await.unwrap();
                Requested::Domain {
                    host,
                    port: u16::from_be_bytes(port),
                }
            }
            atyp => panic!("unexpected address type {atyp}"),
        };

        client
            .write_all(&[5, self.reply, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();

        let target = match &requested {
            Requested::Domain { host, port } => format!("{host}:{port}"),
            Requested::Addr(addr) => addr.to_string(),
        };

        // Reported before relaying, which runs for as long as the connection lives.
        let refused = self.reply != 0;
        let _ = requests.send(requested);

        if !refused {
            let mut upstream = TcpStream::connect(target).await.unwrap();
            let _ = copy_bidirectional(client, &mut upstream).await;
        }
    }
}

/// Reads a length-prefixed string, the shape RFC 1928 and RFC 1929 use throughout.
async fn read_prefixed(stream: &mut TcpStream) -> String {
    let mut len = [0; 1];
    stream.read_exact(&mut len).await.unwrap();
    let mut value = vec![0; usize::from(len[0])];
    stream.read_exact(&mut value).await.unwrap();
    String::from_utf8(value).unwrap()
}

/// Starts an HTTP/1.1 WebSocket echo server and returns the address it listens on.
async fn spawn_echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = service_fn(handle);
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });

    addr
}

/// Upgrades the request and echoes every data frame back.
async fn handle(mut req: Request<Incoming>) -> Result<Response<Empty<Bytes>>, Infallible> {
    let (response, fut) = WebSocket::upgrade_with_options(&mut req, Options::default()).unwrap();

    tokio::spawn(async move {
        let mut ws = fut.await.unwrap();
        while let Some(frame) = ws.next().await {
            if matches!(frame.opcode(), OpCode::Text | OpCode::Binary)
                && ws.send(frame).await.is_err()
            {
                break;
            }
        }
    });

    Ok(response)
}

#[tokio::test]
async fn echoes_through_the_proxy() {
    let echo = spawn_echo_server().await;
    let (proxy, _requested) = MockProxy::default().spawn().await;

    let mut ws = WebSocket::connect(format!("ws://{echo}/chat").parse().unwrap())
        .with_proxy(Proxy::socks5(format!("socks5h://{proxy}").parse().unwrap()).unwrap())
        .await
        .unwrap();

    ws.send(Frame::text("through the tunnel")).await.unwrap();
    let frame = ws.next().await.unwrap();
    assert_eq!(frame.opcode(), OpCode::Text);
    assert_eq!(frame.payload().as_ref(), b"through the tunnel");
}

#[tokio::test]
async fn socks5h_leaves_the_hostname_for_the_proxy_to_resolve() {
    let echo = spawn_echo_server().await;
    let (proxy, mut requested) = MockProxy::default().spawn().await;

    let url = format!("ws://localhost:{}/chat", echo.port());
    let _ws = WebSocket::connect(url.parse().unwrap())
        .with_proxy(Proxy::socks5(format!("socks5h://{proxy}").parse().unwrap()).unwrap())
        .await
        .unwrap();

    assert_eq!(
        requested.recv().await.unwrap(),
        Requested::Domain {
            host: "localhost".to_string(),
            port: echo.port(),
        }
    );
}

#[tokio::test]
async fn socks5_resolves_the_hostname_before_asking_the_proxy() {
    let echo = spawn_echo_server().await;
    let (proxy, mut requested) = MockProxy::default().spawn().await;

    let url = format!("ws://localhost:{}/chat", echo.port());
    let _ws = WebSocket::connect(url.parse().unwrap())
        .with_proxy(Proxy::socks5(format!("socks5://{proxy}").parse().unwrap()).unwrap())
        .await
        .unwrap();

    // Which loopback address `localhost` resolves to is the host's business; that it was
    // resolved here rather than passed on as a name is the point.
    let Requested::Addr(addr) = requested.recv().await.unwrap() else {
        panic!("the hostname was left for the proxy to resolve")
    };
    assert!(addr.ip().is_loopback(), "{addr}");
    assert_eq!(addr.port(), echo.port());
}

#[tokio::test]
async fn a_pinned_address_is_what_the_proxy_is_asked_for() {
    let echo = spawn_echo_server().await;
    let (proxy, mut requested) = MockProxy::default().spawn().await;

    let _ws = WebSocket::connect("ws://example.invalid/chat".parse().unwrap())
        .with_tcp_address(echo)
        .with_proxy(Proxy::socks5(format!("socks5h://{proxy}").parse().unwrap()).unwrap())
        .await
        .unwrap();

    assert_eq!(requested.recv().await.unwrap(), Requested::Addr(echo));
}

#[tokio::test]
async fn credentials_from_the_url_are_accepted() {
    let echo = spawn_echo_server().await;
    let (proxy, _requested) = MockProxy {
        credentials: Some(Credentials {
            username: "user".to_string(),
            password: "p@ss".to_string(),
        }),
        ..MockProxy::default()
    }
    .spawn()
    .await;

    let mut ws = WebSocket::connect(format!("ws://{echo}/chat").parse().unwrap())
        .with_proxy(
            Proxy::socks5(format!("socks5h://user:p%40ss@{proxy}").parse().unwrap()).unwrap(),
        )
        .await
        .unwrap();

    ws.send(Frame::text("authenticated")).await.unwrap();
    assert_eq!(
        ws.next().await.unwrap().payload().as_ref(),
        b"authenticated"
    );
}

#[tokio::test]
async fn wrong_credentials_fail_the_connection() {
    let echo = spawn_echo_server().await;
    let (proxy, _requested) = MockProxy {
        credentials: Some(Credentials {
            username: "user".to_string(),
            password: "right".to_string(),
        }),
        ..MockProxy::default()
    }
    .spawn()
    .await;

    let Err(err) = WebSocket::connect(format!("ws://{echo}/chat").parse().unwrap())
        .with_proxy(
            Proxy::socks5(format!("socks5h://user:wrong@{proxy}").parse().unwrap()).unwrap(),
        )
        .await
    else {
        panic!("the proxy accepted the wrong password")
    };

    assert!(
        matches!(err, WebSocketError::Socks5(Socks5Error::AuthFailed(_))),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_refused_tunnel_surfaces_as_a_typed_error() {
    let echo = spawn_echo_server().await;
    let (proxy, _requested) = MockProxy {
        reply: 5,
        ..MockProxy::default()
    }
    .spawn()
    .await;

    let Err(err) = WebSocket::connect(format!("ws://{echo}/chat").parse().unwrap())
        .with_proxy(Proxy::socks5(format!("socks5h://{proxy}").parse().unwrap()).unwrap())
        .await
    else {
        panic!("the refused tunnel became a connection")
    };

    assert!(
        matches!(
            err,
            WebSocketError::Socks5(Socks5Error::Rejected(ReplyCode::ConnectionRefused))
        ),
        "{err:?}"
    );
}
