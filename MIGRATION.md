# Migration Guide: tokio-tungstenite to yawc

This guide helps you migrate from `tokio-tungstenite` to `yawc`. Both libraries are RFC 6455 compliant and pass the Autobahn test suite, but they have different APIs and feature sets.

## Table of Contents

- [Quick Comparison](#quick-comparison)
- [Basic Client Connection](#basic-client-connection)
- [Basic Server Connection](#basic-server-connection)
- [Message Handling](#message-handling)
- [Compression](#compression)
- [Splitting Streams](#splitting-streams)
- [Axum Integration](#axum-integration)
- [Configuration Options](#configuration-options)
- [Feature Flags](#feature-flags)

## Quick Comparison

| Feature             | tokio-tungstenite | yawc       |
| ------------------- | ----------------- | ---------- |
| RFC 6455 Compliance | Yes               | Yes        |
| Autobahn Tests      | Yes               | Yes        |
| Compression         | No                | Yes        |
| WebAssembly         | No                | Yes        |
| Stream Access       | Direct            | Abstracted |
| reqwest Integration | No                | Yes        |
| Zero-copy Design    | No                | Yes        |

## Basic Client Connection

### tokio-tungstenite

```rust
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures::{StreamExt, SinkExt};

let (ws_stream, _) = connect_async("wss://echo.websocket.org").await?;
let (mut write, mut read) = ws_stream.split();

write.send(Message::Text("Hello".to_string())).await?;

while let Some(msg) = read.next().await {
    let msg = msg?;
    match msg {
        Message::Text(text) => println!("Received: {}", text),
        Message::Binary(data) => println!("Binary: {} bytes", data.len()),
        _ => {}
    }
}
```

### yawc

```rust
use yawc::{WebSocket, Frame, OpCode};
use futures::{StreamExt, SinkExt};

let mut ws = WebSocket::connect("wss://echo.websocket.org".parse()?).await?;

ws.send(Frame::text("Hello")).await?;

while let Some(frame) = ws.next().await {
    let (opcode, _is_fin, payload) = frame.into_parts();
    match opcode {
        OpCode::Text => {
            let text = std::str::from_utf8(&payload)?;
            println!("Received: {}", text);
        }
        OpCode::Binary => println!("Binary: {} bytes", payload.len()),
        _ => {}
    }
}
```

**Key Differences:**

- yawc uses `Frame` instead of `Message`
- URL parsing is explicit in yawc (`.parse()?`)
- yawc returns `Frame` directly (no `Result` wrapping each frame)
- Use `frame.into_parts()` to get `(OpCode, bool, Bytes)` or accessors like `frame.payload()`, `frame.as_str()`

## Basic Server Connection

### tokio-tungstenite

```rust
use tokio_tungstenite::accept_async;
use futures::{StreamExt, SinkExt};

let stream = tokio::net::TcpStream::connect("localhost:8080").await?;
let ws_stream = accept_async(stream).await?;

while let Some(msg) = ws_stream.next().await {
    let msg = msg?;
    ws_stream.send(msg).await?; // Echo
}
```

### yawc

```rust
use yawc::WebSocket;
use hyper::{Request, Response, body::Incoming};
use futures::{StreamExt, SinkExt};

async fn handle_upgrade(req: Request<Incoming>) -> Result<Response<_>> {
    let (response, upfn) = WebSocket::upgrade(req)?;

    tokio::spawn(async move {
        let mut ws = upfn.await.expect("upgrade");

        while let Some(frame) = ws.next().await {
            let _ = ws.send(frame).await; // Echo
        }
    });

    Ok(response)
}
```

**Key Differences:**

- yawc integrates with hyper's HTTP upgrade mechanism
- The upgrade returns a response and a future
- Response must be sent before awaiting the upgrade future

## Message Handling

### tokio-tungstenite

```rust
use tokio_tungstenite::tungstenite::Message;

// Creating messages
let text = Message::Text("Hello".to_string());
let binary = Message::Binary(vec![1, 2, 3]);
let ping = Message::Ping(vec![]);
let pong = Message::Pong(vec![]);
let close = Message::Close(Some(CloseFrame {
    code: CloseCode::Normal,
    reason: "Goodbye".into(),
}));

// Handling messages
match msg {
    Message::Text(text) => { /* ... */ }
    Message::Binary(data) => { /* ... */ }
    Message::Ping(_) => { /* auto-responded */ }
    Message::Pong(_) => { /* ... */ }
    Message::Close(_) => { /* ... */ }
    Message::Frame(_) => { /* raw frame */ }
}
```

### yawc

```rust
use yawc::{Frame, OpCode};
use yawc::close::CloseCode;

// Creating frames
let text = Frame::text("Hello");
let binary = Frame::binary(vec![1, 2, 3]);
let ping = Frame::ping("");
let pong = Frame::pong("");
let close = Frame::close(CloseCode::Normal, b"Goodbye");

// Handling frames - Option 1: Using accessors
match frame.opcode() {
    OpCode::Text => {
        let text = frame.as_str(); // Direct &str access
    }
    OpCode::Binary => {
        let data = frame.payload(); // Bytes reference
    }
    OpCode::Ping => {
        // Pong automatically sent, but ping frame is still returned
        // so you can observe/log it if needed
    }
    OpCode::Pong => { /* ... */ }
    OpCode::Close => {
        let code = frame.close_code();
        let reason = frame.close_reason()?;
    }
    _ => {}
}

// Handling frames - Option 2: Using into_parts() for ownership
let (opcode, _is_fin, payload) = frame.into_parts();
match opcode {
    OpCode::Text => {
        let text = std::str::from_utf8(&payload)?;
    }
    OpCode::Binary => {
        // payload is now owned Bytes
    }
    _ => {}
}
```

**Key Differences:**

- `Frame` instead of `Message`
- `OpCode` enum instead of `Message` variants
- Access payload via `payload()` method or `into_parts()` for ownership
- Helper methods like `as_str()`, `close_code()`, `close_reason()`

## Compression

### tokio-tungstenite

```rust
use tokio_tungstenite::{
    tungstenite::protocol::WebSocketConfig,
    connect_async_with_config,
};

let config = WebSocketConfig {
    max_message_size: Some(64 << 20),
    max_frame_size: Some(16 << 20),
    ..Default::default()
};

let (ws, _) = connect_async_with_config(url, Some(config), false).await?;
```

Note: tokio-tungstenite doesn't directly support compression - you need `tungstenite` with compression features.

### yawc

```rust
use yawc::{WebSocket, Options, CompressionLevel};

let ws = WebSocket::connect("wss://example.com".parse()?)
    .with_options(
        Options::default()
            .with_compression_level(CompressionLevel::default())
            .server_no_context_takeover()  // Reduce memory
            .with_max_payload_read(64 * 1024 * 1024)
    )
    .await?;
```

**Key Differences:**

- yawc has built-in compression support (permessage-deflate)
- More granular control over compression settings
- Context takeover options for memory management
- Window size control with `zlib` feature

## Splitting Streams

### tokio-tungstenite

```rust
let (mut write, mut read) = ws_stream.split();

// Read and write independently
tokio::spawn(async move {
    while let Some(msg) = read.next().await {
        // Handle message
    }
});

tokio::spawn(async move {
    write.send(Message::text("Hello")).await?;
});
```

### yawc

```rust
use futures::StreamExt;

let (mut write, mut read) = ws.split();

// Read and write independently
tokio::spawn(async move {
    while let Some(frame) = read.next().await {
        // Handle frame
    }
});

tokio::spawn(async move {
    write.send(Frame::text("Hello")).await?;
});
```

**Key Differences:**

- Same `futures::StreamExt::split()` approach
- Both maintain protocol handling (ping/pong)
- yawc also has low-level `split_stream()` but it's rarely needed

## Axum Integration

### tokio-tungstenite with axum

```rust
use axum::{
    extract::ws::{WebSocketUpgrade, WebSocket},
    response::Response,
};

async fn ws_handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|socket| handle_socket(socket))
}

async fn handle_socket(mut socket: WebSocket) {
    while let Some(msg) = socket.recv().await {
        let msg = msg.unwrap();
        socket.send(msg).await.unwrap();
    }
}
```

### yawc with axum

```rust
use yawc::{IncomingUpgrade, Options};
use axum::response::Response;

async fn ws_handler(ws: IncomingUpgrade) -> Response {
    let (response, upgrade) = ws
        .upgrade(Options::default())
        .unwrap();

    tokio::spawn(async move {
        let mut ws = upgrade.await.unwrap();

        while let Some(frame) = ws.next().await {
            let _ = ws.send(frame).await;
        }
    });

    response
}
```

**Key Differences:**

- yawc uses `IncomingUpgrade` extractor with `axum` feature
- Explicit `Options` configuration at upgrade time
- Must spawn task manually (more control)
- Returns response directly from handler

## Configuration Options

### tokio-tungstenite

```rust
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

let config = WebSocketConfig {
    max_send_queue: None,
    max_message_size: Some(64 << 20),
    max_frame_size: Some(16 << 20),
    accept_unmasked_frames: false,
};
```

### yawc

```rust
use yawc::{Options, CompressionLevel};

let options = Options::default()
    .with_max_payload_read(64 * 1024 * 1024)
    .with_max_read_buffer(128 * 1024 * 1024)
    .with_compression_level(CompressionLevel::fast())
    .with_utf8()  // Validate UTF-8
    .with_no_delay();  // Disable Nagle's algorithm
```

**Key Differences:**

- yawc focuses on memory limits and compression
- Built-in UTF-8 validation option
- TCP_NODELAY configuration
- Separate read payload and buffer limits

## Feature Flags

### tokio-tungstenite

```toml
[dependencies]
tokio-tungstenite = { version = "0.20", features = [
    "native-tls",  # or "rustls-tls-native-roots"
] }
```

### yawc

```toml
[dependencies]
yawc = { version = "0.3", features = [
    "axum",         # Axum integration
    "reqwest",      # reqwest client support
    "logging",      # Debug logging
    "zlib",         # Advanced compression options
    "json",         # JSON serialization helpers
    "rustls-ring",  # Default: ring crypto
    # "rustls-aws-lc-rs",  # Alternative: AWS-LC crypto
] }
```

**Key Differences:**

- yawc uses rustls by default (tokio-tungstenite requires choosing TLS)
- yawc has framework integrations (axum, reqwest)
- yawc has JSON helpers
- Crypto provider selection for rustls

## Automatic Protocol Handling

Both libraries automatically handle ping/pong, but with a key difference:

### tokio-tungstenite

Ping frames trigger automatic pong responses and are **not** returned via the stream iterator.

### yawc

Ping frames trigger automatic pong responses **and are still returned** to your application, allowing you to observe them:

```rust
use yawc::frame::OpCode;

while let Some(frame) = ws.next().await {
    match frame.opcode() {
        OpCode::Ping => {
            // Pong is sent automatically
            // But you can still see the ping
            log::debug!("Received ping from server");
        }
        OpCode::Text | OpCode::Binary => {
            // Handle data frames
        }
        _ => {}
    }
}
```

This design gives you visibility into connection health checks while still handling the protocol automatically.

## Common Patterns

### Sending Multiple Message Types

**tokio-tungstenite:**

```rust
ws.send(Message::Text("text".into())).await?;
ws.send(Message::Binary(vec![1,2,3])).await?;
ws.send(Message::Ping(vec![])).await?;
```

**yawc:**

```rust
ws.send(Frame::text("text")).await?;
ws.send(Frame::binary(vec![1,2,3])).await?;
ws.send(Frame::ping("")).await?;
```

### Graceful Shutdown

**tokio-tungstenite:**

```rust
ws.send(Message::Close(None)).await?;
ws.close(None).await?;
```

**yawc:**

```rust
use yawc::close::CloseCode;
ws.send(Frame::close(CloseCode::Normal, b"Goodbye")).await?;
// Connection closes automatically
```

### Error Handling

**tokio-tungstenite:**

```rust
match ws.send(msg).await {
    Err(tokio_tungstenite::tungstenite::Error::ConnectionClosed) => {
        // Handle closed connection
    }
    Err(e) => return Err(e.into()),
    Ok(()) => {}
}
```

**yawc:**

```rust
match ws.send(frame).await {
    Err(yawc::WebSocketError::ConnectionClosed) => {
        // Handle closed connection
    }
    Err(e) => return Err(e),
    Ok(()) => {}
}
```

## Migration Checklist

- [ ] Update `Cargo.toml` dependencies
- [ ] Replace `Message` with `Frame`
- [ ] Replace `Message::Text/Binary` with `OpCode::Text/Binary`
- [ ] Update message creation to use `Frame` constructors
- [ ] Change URL string to explicit `.parse()?`
- [ ] Update server upgrade to use hyper integration
- [ ] Configure compression if needed
- [ ] Update Axum handlers if using axum feature
- [ ] Test with your existing test suite
- [ ] Run Autobahn test suite if doing custom protocol work

## Need Help?

- [Documentation](https://docs.rs/yawc)
- [Examples](https://github.com/infinitefield/yawc/tree/master/examples)
- [GitHub Issues](https://github.com/infinitefield/yawc/issues)

## Performance Notes

yawc is designed for high-performance scenarios:

- Zero-copy frame processing where possible
- Efficient handling of fragmented messages
- Built-in compression with configurable memory/CPU trade-offs
- Proven in 24/7 production trading systems

If you're migrating for performance reasons, benchmark your specific use case to validate improvements.
