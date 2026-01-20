/// Example WebSocket client that connects to Bybit's public trade stream
use std::{sync::Arc, time::Duration};

use futures::{SinkExt, StreamExt};
use futures_rustls::TlsConnector;
use rustls_pki_types::ServerName;
use smol::{net::TcpStream, Timer};
use url::Url;
use yawc::{Frame, OpCode, Options, WebSocket};

async fn connect(
    url: Url,
) -> anyhow::Result<impl futures::Stream<Item = Frame> + futures::Sink<Frame>> {
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().unwrap() {
        root_store.add(cert)?;
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let host = url.host_str().ok_or(anyhow::anyhow!("no host!!!"))?;
    let stream = TcpStream::connect(format!("{host}:443")).await?;
    let domain = ServerName::try_from(host)?.to_owned();
    let tls_stream = connector.connect(domain, stream).await?;

    let ws = WebSocket::handshake(
        url,
        Adapter(tls_stream),
        Options::default().with_balanced_compression(),
    )
    .await?;

    Ok(ws)
}

struct Adapter<S>(S);

impl<S> tokio::io::AsyncRead for Adapter<S>
where
    S: futures::AsyncRead + Unpin,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // Get the unfilled portion of the buffer as a mutable slice
        let unfilled = buf.initialize_unfilled();

        // Call the futures AsyncRead trait method
        match std::pin::Pin::new(&mut self.0).poll_read(cx, unfilled) {
            std::task::Poll::Ready(Ok(n)) => {
                // Advance the buffer by the number of bytes read
                buf.advance(n);
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl<S> tokio::io::AsyncWrite for Adapter<S>
where
    S: futures::AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_close(cx)
    }
}

fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Initialize logging
    simple_logger::init_with_level(log::Level::Debug).expect("log");

    smol::block_on(async move {
        // Connect to the WebSocket server with fast compression enabled
        let mut client = connect("wss://stream.bybit.com/v5/public/linear".parse().unwrap())
            .await
            .expect("connection");

        // JSON-formatted subscription request
        let text = r#"{
            "req_id": "1",
            "op": "subscribe",
            "args": [
                "publicTrade.BTCUSDT"
            ]
        }"#;

        // Send subscription request
        let _ = client.send(Frame::text(text)).await;

        // Set up an interval to send pings every 3 seconds
        let mut timer = Timer::interval(Duration::from_secs(3));

        loop {
            tokio::select! {
                // Send a ping on each tick
                _ = timer.next() => {
                    log::debug!("Tick");
                    let _ = client.send(Frame::ping("idk")).await;
                }
                // Handle incoming frames
                frame = client.next() => {
                    if frame.is_none() {
                        log::debug!("Disconnected");
                        break;
                    }

                    let frame = frame.unwrap();
                    let (opcode, _is_fin, body) = frame.into_parts();
                    match opcode {
                        OpCode::Text => {
                            let text = std::str::from_utf8(&body).expect("utf8");
                            log::info!("{text}");
                            let _: serde_json::Value = serde_json::from_str(text).expect("serde");
                        }
                        OpCode::Pong => {
                            let data = std::str::from_utf8(&body).unwrap();
                            log::debug!("Pong: {data}");
                        }
                        _ => {}
                    }
                }
            }
        }
    });
}
