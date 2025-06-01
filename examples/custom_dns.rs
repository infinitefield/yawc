/// Example WebSocket client that uses a custom dns resolver leveraging yawc.
use std::{collections::HashMap, net::SocketAddr};

use futures::{SinkExt, StreamExt};
use yawc::{CompressionLevel, FrameView, Options, WebSocket};

struct CustomDnsResolver {
    overrides: HashMap<String, Vec<SocketAddr>>,
}

impl CustomDnsResolver {
    pub fn resolve(&self, domain: &str) -> Option<Vec<SocketAddr>> {
        self.overrides.get(domain).cloned()
    }
}

#[tokio::main]
async fn main() {
    // Initialize logging
    simple_logger::init_with_level(log::Level::Debug).expect("log");

    let dns = CustomDnsResolver {
        overrides: HashMap::from([(
            "echo.websocket.org".to_owned(),
            vec![
                // this one should fail :(
                "127.0.0.1:9090".parse().unwrap(),
                // this one should not (if you run the echo_server.rs example)
                "127.0.0.1:8080".parse().unwrap(),
            ],
        )]),
    };

    let mut websocket = establish(dns).await.expect("ws");

    log::debug!("Connected");

    let _ = websocket.send(FrameView::text("hello")).await;
    let _ = websocket.next().await;
    let _ = websocket.close().await;
}

async fn establish(dns: CustomDnsResolver) -> Option<WebSocket> {
    let addresses = dns.resolve("echo.websocket.org").expect("addresses");

    for address in addresses {
        log::info!("Connecting to {address}");

        // Connect to the WebSocket server with fast compression enabled
        match WebSocket::connect("ws://echo.websocket.org".parse().unwrap())
            .with_tcp_address(address)
            .with_options(Options::default().with_compression_level(CompressionLevel::fast()))
            .await
        {
            Ok(client) => return Some(client),
            Err(err) => {
                log::warn!("Unable to connect to {address}: {err}");
            }
        }
    }

    None
}
