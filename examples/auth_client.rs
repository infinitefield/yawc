//! Example demonstrating WebSocket client authentication using custom headers.
//!
//! This example shows how to:
//! - Add authentication tokens to the WebSocket handshake
//! - Use custom headers for API keys and bearer tokens
//! - Handle authenticated connections with error recovery

use futures::{SinkExt, StreamExt};
use yawc::{Frame, HttpRequest, OpCode, Options, WebSocket};

#[tokio::main]
async fn main() -> yawc::Result<()> {
    simple_logger::init_with_level(log::Level::Debug).expect("log");

    log::info!("Connecting with Bearer token...");

    let _ws = connect_with_bearer_token(
        "wss://api.example.com/ws".parse().unwrap(),
        "your-secret-token-here",
    )
    .await?;

    log::info!("Connected with Bearer token!");

    log::info!("Connecting with API key...");
    let _ws = connect_with_api_key(
        "wss://api.example.com/ws".parse().unwrap(),
        "your-api-key-here",
    )
    .await?;

    log::info!("Connected with API key!");

    log::info!("Connecting with custom headers...");
    let mut ws = connect_with_custom_headers("wss://api.example.com/ws".parse().unwrap()).await?;
    log::info!("Connected with custom headers!");

    // Use the authenticated WebSocket connection
    ws.send(Frame::text("Hello, authenticated server!")).await?;

    while let Some(frame) = ws.next().await {
        match frame.opcode() {
            OpCode::Text => {
                let text = std::str::from_utf8(frame.payload()).unwrap();
                log::info!("Received: {}", text);
                break;
            }
            OpCode::Close => {
                log::info!("Connection closed by server");
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

/// Connect to a WebSocket server using Bearer token authentication.
///
/// The token is sent in the `Authorization` header as "Bearer <token>".
async fn connect_with_bearer_token(url: url::Url, token: &str) -> yawc::Result<yawc::TcpWebSocket> {
    let request = HttpRequest::builder().header("Authorization", format!("Bearer {}", token));
    WebSocket::connect(url).with_request(request).await
}

/// Connect to a WebSocket server using API key authentication.
///
/// The API key is sent in a custom `X-API-Key` header.
async fn connect_with_api_key(url: url::Url, api_key: &str) -> yawc::Result<yawc::TcpWebSocket> {
    let request = HttpRequest::builder().header("X-API-Key", api_key);
    WebSocket::connect(url).with_request(request).await
}

/// Connect to a WebSocket server with multiple custom authentication headers.
///
/// This example shows how to combine multiple headers for more complex
/// authentication schemes.
async fn connect_with_custom_headers(url: url::Url) -> yawc::Result<yawc::TcpWebSocket> {
    let request = HttpRequest::builder()
        .header("Authorization", "Bearer your-token")
        .header("X-API-Key", "your-api-key")
        .header("X-Client-ID", "client-123")
        .header("X-Session-ID", "session-456");

    WebSocket::connect(url)
        .with_request(request)
        .with_options(Options::default().with_utf8())
        .await
}
