//! Connects through a SOCKS5 proxy and echoes one message.
//!
//! Usage: cargo run --example socks5 -- <proxy-url> [ws-url]
//!
//! The proxy URL is `socks5h://[user:pass@]host:port`, or `socks5://` to resolve the
//! target locally instead of leaving it to the proxy.

use futures::{SinkExt, StreamExt};
use yawc::{Frame, Proxy, WebSocket};

#[tokio::main]
async fn main() -> yawc::Result<()> {
    simple_logger::init_with_level(log::Level::Info).expect("log");

    let mut args = std::env::args().skip(1);
    let proxy_url = args.next().expect("usage: socks5 <proxy-url> [ws-url]");
    let ws_url = args
        .next()
        .unwrap_or_else(|| "wss://echo.websocket.org".to_string());

    let proxy = Proxy::socks5(proxy_url.parse()?)?;
    log::info!("connecting to {ws_url} through {proxy:?}");

    // TLS is negotiated with the target through the tunnel, so the proxy sees only
    // ciphertext for a wss:// URL.
    let mut ws = WebSocket::connect(ws_url.parse()?)
        .with_proxy(proxy)
        .await?;

    ws.send(Frame::text("hello through the proxy")).await?;

    while let Some(frame) = ws.next().await {
        log::info!("received {:?}: {}", frame.opcode(), frame.as_str());
    }

    Ok(())
}
