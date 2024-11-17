/// Example WebSocket client that connects to Bybit's public trade stream
use std::{sync::Arc, time::Duration};

use futures::{SinkExt, StreamExt};
use tokio::time::interval;
use tokio_rustls::{
    rustls::{self, pki_types::TrustAnchor},
    TlsConnector,
};
use yawc::{
    frame::{FrameView, OpCode},
    CompressionLevel, Options, WebSocket,
};

#[tokio::main]
async fn main() {
    // Initialize logging
    simple_logger::init_with_level(log::Level::Debug).expect("log");

    // Connect to the WebSocket server with fast compression enabled
    let mut client = WebSocket::connect_with_options(
        "wss://stream.bybit.com/v5/public/linear".parse().unwrap(),
        Some(tls_connector()),
        Options::default().with_compression_level(CompressionLevel::fast()),
    )
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
    let _ = client.send(FrameView::text(text)).await;

    // Set up an interval to send pings every 3 seconds
    let mut ival = interval(Duration::from_secs(3));

    loop {
        tokio::select! {
            // Send a ping on each tick
            _ = ival.tick() => {
                log::debug!("Tick");
                let _ = client.send(FrameView::ping("idk")).await;
            }
            // Handle incoming frames
            frame = client.next() => {
                if frame.is_none() {
                    log::debug!("Disconnected");
                    break;
                }

                let frame = frame.unwrap();
                let (opcode, body) = (frame.opcode, frame.payload);
                match opcode {
                    OpCode::Text => {
                        let text = std::str::from_utf8(&body).expect("utf8");
                        log::info!("{text}");
                        let _: serde_json::Value = serde_json::from_str(text).expect("serde");
                    }
                    OpCode::Pong => {
                        let data = std::str::from_utf8(&body).unwrap();
                        log::debug!("Pong: {}", data);
                    }
                    OpCode::Close => {
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Creates a TLS connector with root certificates for secure WebSocket connections
///
/// Returns a TlsConnector configured with the system root certificates
/// and no client authentication.
fn tls_connector() -> TlsConnector {
    let mut root_cert_store = rustls::RootCertStore::empty();
    root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| TrustAnchor {
        subject: ta.subject.clone(),
        subject_public_key_info: ta.subject_public_key_info.clone(),
        name_constraints: ta.name_constraints.clone(),
    }));
    // config.dangerous()... to ignore the cert verification

    TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(root_cert_store)
            .with_no_client_auth(),
    ))
}
