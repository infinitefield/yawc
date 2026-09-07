use std::{process::Stdio, sync::Arc, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine};
use futures::SinkExt;
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::{Child, Command},
    time::timeout,
};
use yawc::{
    frame::{Frame, OpCode},
    Options, Role, WebSocket,
};

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_yawcc"));
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

async fn client(url: &str, args: &[&str], input: Option<&str>) -> std::process::Output {
    let mut command = command();
    command.arg("c").arg(url).args(args);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .await
            .unwrap();
    }
    timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("CLI hung")
        .unwrap()
}

fn success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn server(args: &[&str]) -> (Child, String) {
    let mut child = command()
        .args(["s", "--listen", "127.0.0.1:0"])
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    timeout(Duration::from_secs(5), stderr.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let url = line
        .trim()
        .strip_prefix("WebSocket server listening on: ")
        .unwrap()
        .to_owned();
    tokio::spawn(async move {
        let mut sink = tokio::io::sink();
        let _ = tokio::io::copy(&mut stderr, &mut sink).await;
    });
    (child, url)
}

async fn request<S: AsyncRead + Unpin>(stream: &mut S) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 32768);
        bytes.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap()
}

fn header<'a>(request: &'a str, key: &str) -> Option<&'a str> {
    request
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.trim())
}

async fn upgrade<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    mut stream: S,
    extra: &str,
) -> (WebSocket<S>, String) {
    let request = request(&mut stream).await;
    let key = header(&request, "sec-websocket-key").unwrap();
    let accept = STANDARD.encode(Sha1::digest(format!(
        "{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
    )));
    stream.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n{extra}\r\n").as_bytes()).await.unwrap();
    (
        WebSocket::from_stream(stream, Role::Server, Options::default().with_utf8()).unwrap(),
        request,
    )
}

async fn echo<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(mut ws: WebSocket<S>) {
    loop {
        let frame = ws.next_frame().await.unwrap();
        if frame.opcode() == OpCode::Close {
            let _ = ws.next_frame().await; // Flush automatic close reply.
            let _ = ws.close().await;
            return;
        }
        if matches!(frame.opcode(), OpCode::Text | OpCode::Binary) {
            ws.send(frame).await.unwrap();
        }
    }
}

async fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    (listener, url)
}

#[tokio::test]
async fn pipelines_preserve_urls_blank_messages_and_final_line() {
    let (_server, url) = server(&[]).await;
    let input = "{\"url\":\"https://example.com\"}\n\nlast // literal";
    let output = client(&url, &["--wait", "100ms"], Some(input)).await;
    success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{input}\n")
    );
}

#[tokio::test]
async fn execute_comments_json_and_timestamps_compose() {
    let (_server, url) = server(&[]).await;
    let output = client(
        &url,
        &[
            "-x",
            "{\"url\":\"https://example.com\"} // note",
            "-x",
            "// skipped",
            "-x",
            "{\"n\":2}",
            "--comments",
            "--input-as-json",
            "--include-time",
            "-w",
            "0.1",
        ],
        None,
    )
    .await;
    success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"url\": \"https://example.com\""));
    assert!(stdout.contains("\"n\": 2"));
    assert!(!stdout.contains("note"));
    assert!(stdout.lines().next().unwrap().as_bytes()[2] == b':');
}

#[tokio::test]
async fn input_and_connection_errors_exit_nonzero_without_panics() {
    for args in [
        vec!["-H", "bad-header"],
        vec!["-H", "Bad Header: x"],
        vec!["-H", "X-Test: a\r\nb"],
        vec!["--slash", "-x", "/close 1006"],
        vec!["--auth", "bad"],
        vec!["--wait", "-2"],
        vec!["--subprotocol", "bad,protocol"],
    ] {
        let output = client("ws://127.0.0.1:1/", &args, None).await;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
        assert!(output.stdout.is_empty());
    }
    let output = client("ws://127.0.0.1:1/", &[], None).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("TCP connection failed"));
    let (_server, url) = server(&[]).await;
    let output = client(&url, &["--input-as-json", "-x", "not json"], None).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("received invalid JSON"));
}

#[tokio::test]
async fn headers_auth_and_subprotocol_are_sent_and_negotiated() {
    let (listener, url) = listener().await;
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (ws, request) = upgrade(stream, "Sec-WebSocket-Protocol: chat\r\n").await;
        assert_eq!(
            header(&request, "authorization"),
            Some("Basic dXNlcjpwYXNz")
        );
        assert_eq!(header(&request, "origin"), Some("https://example.com"));
        assert_eq!(header(&request, "x-test"), Some("one"));
        assert_eq!(
            header(&request, "sec-websocket-protocol"),
            Some("chat, other")
        );
        echo(ws).await;
    });
    let output = client(
        &url,
        &[
            "--auth",
            "user:pass",
            "-o",
            "https://example.com",
            "-H",
            "X-Test: one",
            "-s",
            "chat",
            "-s",
            "other",
            "-x",
            "hello",
            "-w",
            "50ms",
        ],
        None,
    )
    .await;
    success(&output);
    assert_eq!(output.stdout, b"hello\n");
    peer.await.unwrap();
}

#[tokio::test]
async fn server_paths_are_exact_and_server_negotiates_subprotocols() {
    let (_server, url) = server(&["--path", "/ws", "-s", "chat"]).await;
    let output = client(
        &format!("{url}?token=1"),
        &["-s", "chat", "-x", "ok", "-w", "50ms"],
        None,
    )
    .await;
    success(&output);
    let output = client(&format!("{url}-wrong"), &["-s", "chat"], None).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("404"));
}

#[tokio::test]
async fn ping_pong_and_close_payloads_are_visible_only_on_stderr() {
    let (_server, url) = server(&[]).await;
    let output = client(
        &url,
        &["--slash", "-P", "-x", "/ping hi", "-w", "100ms"],
        None,
    )
    .await;
    success(&output);
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Pong: aGk="));
    let output = client(&url, &["--slash", "-x", "/close 1000, finished"], None).await;
    success(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("code=1000, reason=finished"));
}

#[tokio::test]
async fn binary_formats_preserve_payloads() {
    for (format, expected) in [
        ("hex", &b"00ff0a\n"[..]),
        ("base64", &b"AP8K\n"[..]),
        ("raw", &b"\0\xff\n"[..]),
    ] {
        let (listener, url) = listener().await;
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut ws, _) = upgrade(stream, "").await;
            ws.send(Frame::binary(vec![0, 255, 10])).await.unwrap();
            echo(ws).await;
        });
        let output = client(&url, &["--binary", format, "-w", "50ms"], None).await;
        success(&output);
        assert_eq!(output.stdout, expected);
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn invalid_utf8_and_transport_loss_are_failures() {
    for payload in [&b"\x81\x01\xff"[..], &b""[..]] {
        let (listener, url) = listener().await;
        let payload = payload.to_vec();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (ws, _) = upgrade(stream, "").await;
            if payload.is_empty() {
                drop(ws);
            } else {
                let mut ws = ws;
                ws.send(Frame::text(vec![255])).await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let output = client(&url, &["-w", "-1"], None).await;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("receive frame"));
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn redirects_strip_cross_origin_headers_and_enforce_limit() {
    let (target, target_url) = listener().await;
    let (redirect, redirect_url) = listener().await;
    let peer = tokio::spawn(async move {
        let (stream, _) = target.accept().await.unwrap();
        let (ws, request) = upgrade(stream, "").await;
        assert!(header(&request, "authorization").is_none());
        assert!(header(&request, "cookie").is_none());
        assert!(header(&request, "x-api-key").is_none());
        echo(ws).await;
    });
    let redirect_peer = tokio::spawn(async move {
        let (mut stream, _) = redirect.accept().await.unwrap();
        let request = request(&mut stream).await;
        assert_eq!(header(&request, "x-api-key"), Some("secret"));
        stream
            .write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let output = client(
        &redirect_url,
        &[
            "-L",
            "--auth",
            "a:b",
            "-H",
            "Cookie: secret",
            "-H",
            "X-Api-Key: secret",
            "-x",
            "ok",
            "-w",
            "50ms",
        ],
        None,
    )
    .await;
    success(&output);
    peer.await.unwrap();
    redirect_peer.await.unwrap();

    let (listener, url) = listener().await;
    let peer = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        }
    });
    let output = client(&url, &["-L", "--max-redirects", "1"], None).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("maximum redirects"));
    peer.await.unwrap();
}

#[tokio::test]
async fn http_proxy_uses_connect_and_separates_proxy_credentials() {
    let (listener, proxy_url) = listener().await;
    let proxy = proxy_url.replace("ws://", "http://user:p%40ss@");
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let connect = request(&mut stream).await;
        assert!(connect.starts_with("CONNECT example.invalid:80 HTTP/1.1\r\n"));
        assert_eq!(
            header(&connect, "proxy-authorization"),
            Some("Basic dXNlcjpwQHNz")
        );
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        let (ws, request) = upgrade(stream, "").await;
        assert!(header(&request, "proxy-authorization").is_none());
        assert_eq!(header(&request, "host"), Some("example.invalid"));
        echo(ws).await;
    });
    let output = client(
        "ws://example.invalid/",
        &["--proxy", &proxy, "-x", "through proxy", "-w", "50ms"],
        None,
    )
    .await;
    success(&output);
    assert_eq!(output.stdout, b"through proxy\n");
    peer.await.unwrap();
}

#[tokio::test]
async fn socks5h_preserves_proxy_dns_and_authentication() {
    let (listener, url) = listener().await;
    let proxy = url.replace("ws://", "socks5h://user:p%40ss@");
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 5);
        let methods = stream.read_u8().await.unwrap();
        let mut buffer = vec![0; methods as usize];
        stream.read_exact(&mut buffer).await.unwrap();
        assert!(buffer.contains(&2));
        stream.write_all(&[5, 2]).await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 1);
        let n = stream.read_u8().await.unwrap();
        let mut user = vec![0; n as usize];
        stream.read_exact(&mut user).await.unwrap();
        let n = stream.read_u8().await.unwrap();
        let mut password = vec![0; n as usize];
        stream.read_exact(&mut password).await.unwrap();
        assert_eq!(user, b"user");
        assert_eq!(password, b"p@ss");
        stream.write_all(&[1, 0]).await.unwrap();
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).await.unwrap();
        assert_eq!(prefix, [5, 1, 0, 3]);
        let n = stream.read_u8().await.unwrap();
        let mut host = vec![0; n as usize];
        stream.read_exact(&mut host).await.unwrap();
        assert_eq!(host, b"example.invalid");
        assert_eq!(stream.read_u16().await.unwrap(), 80);
        stream
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        let (ws, _) = upgrade(stream, "").await;
        echo(ws).await;
    });
    let output = client(
        "ws://example.invalid/",
        &["--proxy", &proxy, "-x", "socks", "-w", "50ms"],
        None,
    )
    .await;
    success(&output);
    assert_eq!(output.stdout, b"socks\n");
    peer.await.unwrap();
}

#[tokio::test]
async fn connection_deadline_covers_stalled_handshake() {
    let (listener, url) = listener().await;
    let peer = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let output = client(&url, &["--timeout", "50ms"], None).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("connection deadline exceeded"));
    peer.abort();
}

#[tokio::test]
async fn tls_custom_ca_no_check_and_client_certificates() {
    use tokio_rustls::{
        rustls::{self, pki_types::PrivatePkcs8KeyDer},
        TlsAcceptor,
    };
    let dir = tempfile::tempdir().unwrap();
    let server_identity = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let client_identity = rcgen::generate_simple_self_signed(vec!["client".into()]).unwrap();
    let ca_path = dir.path().join("ca.pem");
    let cert_path = dir.path().join("client.pem");
    let key_path = dir.path().join("client-key.pem");
    std::fs::write(&ca_path, server_identity.cert.pem()).unwrap();
    std::fs::write(&cert_path, client_identity.cert.pem()).unwrap();
    std::fs::write(&key_path, client_identity.key_pair.serialize_pem()).unwrap();
    for mode in ["untrusted", "ca", "no-check", "mutual"] {
        let builder = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap();
        let builder = if mode == "mutual" {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(client_identity.cert.der().clone()).unwrap();
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
            )
            .build()
            .unwrap();
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };
        let config = builder
            .with_single_cert(
                vec![server_identity.cert.der().clone()],
                PrivatePkcs8KeyDer::from(server_identity.key_pair.serialize_der()).into(),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let (listener, url) = listener().await;
        let address = listener.local_addr().unwrap().to_string();
        let url = url.replace("ws://127.0.0.1", "wss://localhost");
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            match acceptor.accept(stream).await {
                Ok(stream) => {
                    let (ws, _) = upgrade(stream, "").await;
                    echo(ws).await;
                }
                Err(_) => assert_eq!(mode, "untrusted"),
            }
        });
        let mut args = vec!["--tcp-host", address.as_str(), "-x", "secure", "-w", "50ms"];
        if matches!(mode, "ca" | "mutual") {
            args.extend(["--ca", ca_path.to_str().unwrap()]);
        }
        if mode == "no-check" {
            args.push("--no-check");
        }
        if mode == "mutual" {
            args.extend([
                "--cert",
                cert_path.to_str().unwrap(),
                "--key",
                key_path.to_str().unwrap(),
            ]);
        }
        let output = client(&url, &args, None).await;
        if mode == "untrusted" {
            assert!(!output.status.success());
        } else {
            success(&output);
            assert_eq!(output.stdout, b"secure\n");
        }
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn interactive_server_receives_and_broadcasts_without_echoing() {
    let (mut server, url) = server(&["--interactive"]).await;
    let mut ws = WebSocket::connect(url.parse().unwrap()).await.unwrap();
    let mut stdout = BufReader::new(server.stdout.take().unwrap());
    ws.send(Frame::text("from client")).await.unwrap();
    let mut line = String::new();
    timeout(Duration::from_secs(2), stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(line, "from client\n");
    server
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"from server\n")
        .await
        .unwrap();
    let frame = timeout(Duration::from_secs(2), ws.next_frame())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.payload().as_ref(), b"from server");
    drop(server.stdin.take());
    assert_eq!(
        timeout(Duration::from_secs(2), ws.next_frame())
            .await
            .unwrap()
            .unwrap()
            .opcode(),
        OpCode::Close
    );
    let _ = ws.next_frame().await;
    assert!(timeout(Duration::from_secs(5), server.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
}

#[tokio::test]
async fn malformed_handshakes_are_rejected_before_sending_messages() {
    for (extra, expected) in [
        ("Sec-WebSocket-Accept: duplicate\r\n", "Sec-WebSocket-Accept"),
        ("Sec-WebSocket-Protocol: unoffered\r\n", "unoffered subprotocol"),
        ("Sec-WebSocket-Extensions: unknown\r\n", "unoffered extension"),
        ("Sec-WebSocket-Extensions: permessage-deflate; server_max_window_bits=99\r\n", "invalid compression window"),
        ("Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits=10\r\n", "unoffered compression parameter"),
        ("Sec-WebSocket-Extensions: permessage-deflate; client_no_context_takeover; client_no_context_takeover\r\n", "duplicate compression parameter"),
    ] {
        let (listener, url) = listener().await;
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut ws, _) = upgrade(stream, extra).await;
            assert!(ws.next_frame().await.is_err());
        });
        let output = client(&url, &["-x", "must not be sent"], None).await;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected), "{}", String::from_utf8_lossy(&output.stderr));
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn close_codes_are_reported_and_peer_errors_fail() {
    for (code, should_succeed) in [(1000, true), (1001, true), (1011, false), (4000, false)] {
        let (listener, url) = listener().await;
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut ws, _) = upgrade(stream, "").await;
            ws.send(Frame::close(code.into(), "peer reason"))
                .await
                .unwrap();
            assert_eq!(ws.next_frame().await.unwrap().opcode(), OpCode::Close);
        });
        let output = client(&url, &["-w", "-1"], None).await;
        assert_eq!(output.status.success(), should_succeed);
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains(&format!("code={code}, reason=peer reason")));
        peer.await.unwrap();
    }
    let (_server, url) = server(&[]).await;
    let output = client(&url, &["--slash", "-x", "/close 4000, custom"], None).await;
    success(&output);
}

#[tokio::test]
async fn eof_waits_for_a_delayed_reply_and_closes_once() {
    let (listener, url) = listener().await;
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut ws, _) = upgrade(stream, "").await;
        let frame = ws.next_frame().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        ws.send(frame).await.unwrap();
        assert_eq!(ws.next_frame().await.unwrap().opcode(), OpCode::Close);
        let _ = ws.next_frame().await; // Flush the reply.
    });
    let output = client(&url, &["-w", "150ms"], Some("delayed\n")).await;
    success(&output);
    assert_eq!(output.stdout, b"delayed\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("code=1000"));
    peer.await.unwrap();
}

#[tokio::test]
async fn socks5_resolves_locally_and_accepts_an_empty_password() {
    let (listener, url) = listener().await;
    let proxy = url.replace("ws://", "socks5://user@");
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 5);
        let n = stream.read_u8().await.unwrap();
        let mut methods = vec![0; n as usize];
        stream.read_exact(&mut methods).await.unwrap();
        stream.write_all(&[5, 2]).await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 1);
        let n = stream.read_u8().await.unwrap();
        let mut user = vec![0; n as usize];
        stream.read_exact(&mut user).await.unwrap();
        assert_eq!(user, b"user");
        assert_eq!(stream.read_u8().await.unwrap(), 0);
        stream.write_all(&[1, 0]).await.unwrap();
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix[..3], &[5, 1, 0]);
        assert!(
            matches!(prefix[3], 1 | 4),
            "local DNS must send an IP address"
        );
        let mut address = vec![0; if prefix[3] == 1 { 4 } else { 16 }];
        stream.read_exact(&mut address).await.unwrap();
        assert_eq!(stream.read_u16().await.unwrap(), 80);
        stream
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        let (ws, _) = upgrade(stream, "").await;
        echo(ws).await;
    });
    let output = client(
        "ws://localhost/",
        &["--proxy", &proxy, "-x", "socks", "-w", "50ms"],
        None,
    )
    .await;
    success(&output);
    peer.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn remote_close_restores_terminal_while_readline_is_blocked() {
    use nix::sys::termios::{tcgetattr, LocalFlags};
    let pty = nix::pty::openpty(None, None).unwrap();
    let original = tcgetattr(&pty.slave).unwrap();
    let (listener, url) = listener().await;
    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut ws, _) = upgrade(stream, "").await;
        close_rx.await.unwrap();
        ws.send(Frame::close(1000.into(), "finished"))
            .await
            .unwrap();
        assert_eq!(ws.next_frame().await.unwrap().opcode(), OpCode::Close);
    });
    let mut child = command()
        .args(["c", &url, "--no-history"])
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
        .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
        .spawn()
        .unwrap();
    timeout(Duration::from_secs(3), async {
        while tcgetattr(&pty.slave)
            .unwrap()
            .local_flags
            .contains(LocalFlags::ICANON)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("readline never entered raw mode");
    close_tx.send(()).unwrap();
    assert!(timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    assert_eq!(tcgetattr(&pty.slave).unwrap(), original);
    peer.await.unwrap();
}
