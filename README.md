# yawc (Yet another websocket crate)

Yet another websocket crate. But a fast, secure, and RFC-compliant WebSocket implementation for Rust with advanced compression support.

[![Crates.io](https://img.shields.io/crates/v/yawc.svg)](https://crates.io/crates/yawc)
[![Documentation](https://docs.rs/yawc/badge.svg)](https://docs.rs/yawc)
[![License](https://img.shields.io/badge/license-AGPL%20v3.0-blue.svg)](LICENSE)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org)

## Features

- **Full RFC 6455 Compliance**: Complete implementation of the WebSocket protocol
- **Secure by Default**: Built-in TLS support with `rustls`
- **Advanced Compression**: Support for permessage-deflate (RFC 7692)
- **Zero-Copy Design**: Efficient frame processing with minimal allocations
- **Automatic Frame Management**: Handles control frames and fragmentation
- **Autobahn Test Suite**: Passes all test cases for both client and server modes

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
yawc = "0.1"
```

### Client Example

```rust
use yawc::{WebSocket, Options, frame::FrameView, Result};

#[tokio::main]
async fn main() -> Result<()> {
    // Connect with default options
    let mut ws = WebSocket::connect(
        "wss://echo.websocket.org".parse()?,
        Some(tls_connector())
    ).await?;

    // Send and receive messages
    ws.send(FrameView::text("Hello WebSocket!")).await?;

    while let Some(frame) = ws.next().await {
        match frame.opcode {
            OpCode::Text => println!("Received: {}", std::str::from_utf8(&frame.payload)?),
            OpCode::Binary => println!("Received binary: {} bytes", frame.payload.len()),
            _ => {} // Handle control frames automatically
        }
    }

    Ok(())
}
```

### Server Example

```rust
use hyper::{Request, Response};
use yawc::{upgrade, WebSocket};

async fn handle_upgrade(req: Request<Body>) -> Result<Response<Body>> {
    // Upgrade the connection
    let (response, upfn) = upgrade(req)?;

    // Handle the WebSocket connection in a separate task
    tokio::spawn(async move {
        let mut ws = upfn.await?;

        while let Some(frame) = ws.next().await {
            // Echo the received frames back to the client
            ws.send(frame).await?;
        }
    });

    Ok(response)
}
```

## Feature Flags

- `reqwest`: Use reqwest as the HTTP client
- `zlib`: Enable advanced compression options with zlib
- `logging`: Enable debug logging for connection events
- `axum`: Enable integration with the Axum web framework

### Axum Server Example

```rust
use axum::{
    routing::get,
    Router,
};
use yawc::{IncomingUpgrade, Options};

async fn websocket_handler(ws: IncomingUpgrade) -> axum::response::Response {
    let options = Options::default()
        .with_compression_level(CompressionLevel::default())
        .with_utf8();

    let (response, ws) = ws.upgrade(options).unwrap();

    // Handle the WebSocket connection in a separate task
    tokio::spawn(async move {
        if let Ok(mut ws) = ws.await {
            while let Ok(frame) = ws.next_frame().await {
                // Echo the received frames back to the client
                let _ = ws.send(frame).await;
            }
        }
    });

    response
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/ws", get(websocket_handler));

    let addr = ([127, 0, 0, 1], 3000).into();
    println!("WebSocket server listening on {}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
```

To use the Axum integration, add this to your `Cargo.toml`:

```toml
[dependencies]
yawc = { version = "0.1", features = ["axum"] }
axum = "0.7"
```

## Advanced Features

### Compression Control

Fine-tune compression settings for optimal performance:

```rust
use yawc::{Options, DeflateOptions, CompressionLevel};

let options = Options::default()
    .with_compression_level(CompressionLevel::default())
    .server_no_context_takeover()  // Optimize memory usage
    .with_client_max_window_bits(11);  // Control compression window
```

### Split Streams

Split the WebSocket for independent reading and writing:

```rust
let (mut read, mut write) = ws.split();

// Read and write concurrently
tokio::join!(
    async move { while let Some(frame) = read.next().await { /* ... */ } },
    async move { write.send(FrameView::text("Hello")).await? }
);
```

### Custom Frame Handling

Process frames manually when needed:

```rust
match frame.opcode {
    OpCode::Ping => {
        // Automatic pong responses
        println!("Received ping");
    }
    OpCode::Close => {
        // Handle close frames
        let code = u16::from_be_bytes(frame.payload[0..2].try_into()?);
        println!("Connection closing with code: {}", code);
    }
    _ => { /* Handle data frames */ }
}
```

## Performance Considerations

- Uses zero-copy frame processing where possible
- Efficient handling of fragmented messages
- Configurable compression levels for bandwidth/CPU tradeoffs
- Memory-efficient compression contexts

## Safety and Security

- Maximum payload size limits (configurable, default 2MB)
- Automatic masking of client frames
- Optional UTF-8 validation for text frames
- Protection against memory exhaustion attacks
- TLS support for secure connections

## Motivation

While several WebSocket libraries exist for Rust's async ecosystem,
none of them provide the full combination of features needed for high-performance,
production-ready applications while maintaining a simple API.
Existing libraries lack proper full-duplex stream support, zero-copy operations,
or compression capabilities - or implement these features with complex, difficult-to-use APIs.
This library aims to provide all these critical features with an ergonomic interface
that makes WebSocket development straightforward and efficient.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.

## License

This project is licensed under the GNU Affero General Public License v3.0 (AGPL-3.0).
Under the terms of this license, any modifications made to the code must be made available publicly under the same license.
This ensures the software remains free and open source, with all improvements benefiting the community.
For more details, see ([LICENSE](LICENSE) or https://www.gnu.org/licenses/agpl-3.0.en.html)

## Acknowledgments

Special thanks to:

- The Tungstenite project for inspiration on close codes
- The fastwebsockets project which served as inspiration and source for many implementations
- The Autobahn test suite for protocol compliance verification
