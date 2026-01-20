# yawc

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
- **WebAssembly Support**: Works seamlessly in WASM environments for browser-based applications (both text and binary modes supported)

## About compression

yawc supports websocket compression through the [Options](https://docs.rs/yawc/latest/yawc/struct.Options.html) struct.
You can use `Options.with_compression_level(CompressLevel::fast())` in order to configure compression.

```rust
let mut client = WebSocket::connect("wss://my-websocket-server.com".parse().unwrap())
    .with_options(Options::default().with_compression_level(CompressionLevel::fast()))
    .await;
```

The `zlib` feature is NOT mandatory to enable compression. `zlib` is configured as a feature for the [window](https://docs.rs/yawc/latest/yawc/struct.Options.html#method.with_client_max_window_bits) parameters.
By default yawc uses [flate2](https://docs.rs/flate2/) with the miniz_oxide backend. An implementation of miniz using Rust.

## Migrating from tokio-tungstenite?

See our comprehensive [Migration Guide](MIGRATION.md) for step-by-step instructions on migrating from tokio-tungstenite to yawc.

## Which crate should I use?

When choosing a WebSocket implementation, many developers default to [tokio-tungstenite](https://github.com/snapview/tokio-tungstenite).
As the most stable and widely-used crate in the ecosystem, it provides excellent abstraction over the
WebSocket protocol through its [WebSocketStream](https://docs.rs/tokio-tungstenite/latest/tokio_tungstenite/struct.WebSocketStream.html) type,
which allows projects to implement custom protocols via its generic `<S>` parameter.

While yawc doesn't expose the underlying stream directly,
it provides access to `poll` methods via [futures::Stream](https://docs.rs/futures/latest/futures/prelude/trait.Stream.html)
and [futures::Sink](https://docs.rs/futures/latest/futures/prelude/trait.Sink.html) implementations.
Key features include built-in compression support, zero-copy operations where possible, and first-class WebAssembly support for UI development.
Beyond passing comprehensive test suites including Autobahn,
yawc has proven its reliability in production environments powering 24/7 market trading systems.

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
yawc = "0.2"
```

### Client Example

```rust
use futures::SinkExt;
use futures::StreamExt;
use yawc::{frame::Frame, frame::OpCode, Options, Result, WebSocket};

#[tokio::main]
async fn main() -> Result<()> {
    // Connect with default options
    let mut ws = WebSocket::connect("wss://echo.websocket.org".parse()?).await?;

    // Send and receive messages
    ws.send(Frame::text("Hello WebSocket!")).await?;

    while let Some(frame) = ws.next().await {
        match frame.opcode() {
            OpCode::Text => println!("Received: {}", frame.as_str()),
            OpCode::Binary => println!("Received binary: {} bytes", frame.payload().len()),
            _ => {} // Handle control frames automatically
        }
    }

    Ok(())
}
```

```toml
[dependencies]
yawc = { version = "0.2" }
futures = { version = "0.3", default-features = false, features = ["std"] }
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros"] }
```

### Server Example

```rust
use hyper::{Request, Response, body::Incoming};
use futures::StreamExt;
use futures::SinkExt;
use bytes::Bytes;
use http_body_util::Empty;
use yawc::{WebSocket, Result};

async fn handle_upgrade(req: Request<Incoming>) -> Result<Response<Empty<Bytes>>> {
    // Upgrade the connection
    let (response, upfn) = WebSocket::upgrade(req)?;

    // Handle the WebSocket connection in a separate task
    tokio::spawn(async move {
        let mut ws = upfn.await.expect("upgrade");

        while let Some(frame) = ws.next().await {
            // Echo the received frames back to the client
            let _ = ws.send(frame).await;
        }
    });

    Ok(response)
}

#[tokio::main]
async fn main() {
    // configure the server
}
```

```toml
[dependencies]
yawc = {version = "0.2", features = ["reqwest"] }
futures = { version = "0.3.31", default-features = false, features = ["std"] }
tokio = { version = "1.41.1", features = ["rt", "rt-multi-thread", "macros"] }
hyper = { version = "1.5.0", features = ["http1", "server"] }
http-body-util = "0.1.2"
bytes = "1.8.0"
```

The [`examples`](https://github.com/infinitefield/yawc/tree/master/examples) directory contains several documented and runnable examples showcasing advanced WebSocket functionality.
You can find a particularly comprehensive example in the [`axum_proxy`](https://github.com/infinitefield/yawc/tree/master/examples/axum_proxy) implementation, which demonstrates:

- Building a WebSocket broadcast server that efficiently relays messages between multiple connected clients
- Creating a reverse proxy that transparently forwards WebSocket connections to upstream servers
- Proper connection lifecycle management and error handling Integration with the Axum web framework for robust HTTP request handling
- Advanced usage patterns like connection pooling and message filtering

These examples serve as practical reference implementations for common WebSocket architectural patterns and best practices using yawc.

## Feature Flags

- `reqwest`: Use reqwest as the HTTP client
- `axum`: Enable integration with the Axum web framework
- `logging`: Enable debug logging for connection events
- `zlib`: Enable advanced compression options with zlib (not recommended unless you know what you are doing). Without this option, yawc will use miniz_oxide, a Rust deflate implementation.
- `rustls-ring`: Enable the fallback rustls crypto provider based on `ring`
- `rustls-aws-lc-rs`: Enable the fallback rustls crypto provider based on `aws-lc-rs`

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

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

To use the Axum integration, add this to your `Cargo.toml`:

```toml
[dependencies]
yawc = { version = "0.2", features = ["axum"] }
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
    .with_client_max_window_bits(11);  // Control compression window (requires zlib feature)
```

### Split Streams

Split the WebSocket for independent reading and writing:

```rust
let (mut write, mut read) = ws.split();

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

## Architecture

yawc implements a clean layered architecture for WebSocket message processing:

```
┌─────────────────────────────────────────────────────────────┐
│                      Application Layer                       │
│                  (Your WebSocket Application)                │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                      WebSocket Layer                         │
│          • Decompression (permessage-deflate RFC 7692)      │
│          • UTF-8 validation for text frames                  │
│          • Protocol control (Ping/Pong, Close)               │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                       ReadHalf Layer                         │
│          • Fragment assembly (RFC 6455 fragmentation)        │
│          • Fragment timeout management                        │
│          • Maximum message size enforcement                   │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                      Tokio Codec Layer                       │
│          • Frame decoding from raw bytes                     │
│          • Frame encoding to raw bytes                        │
│          • Masking/unmasking                                  │
│          • Header parsing (FIN, RSV, OpCode)                 │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
                    Network (TCP/TLS)
```

### Data Flow for Compressed Fragmented Messages

When receiving a compressed fragmented message (e.g., 8KB payload split into 256-byte fragments):

1. **Codec Layer**: Decodes each frame from bytes
   - Frame 1: `OpCode::Text, RSV1=1 (compressed), FIN=0` → Returns individual frame
   - Frame 2: `OpCode::Continuation, RSV1=0, FIN=0` → Returns individual frame
   - Frame 3: `OpCode::Continuation, RSV1=0, FIN=1` → Returns individual frame

2. **ReadHalf Layer**: Assembles fragments into complete message
   - Accumulates fragments, tracking `is_compressed` flag from first frame
   - On final fragment (`FIN=1`), concatenates all payloads
   - Returns complete frame with assembled compressed payload

3. **WebSocket Layer**: Decompresses and validates
   - Decompresses the complete assembled payload (RFC 7692)
   - Validates UTF-8 for text frames
   - Returns final frame to application

This architecture ensures:

- **RFC 6455 compliance**: Proper fragmentation handling
- **RFC 7692 compliance**: Correct permessage-deflate decompression
- **Clean separation**: Each layer has a single, well-defined responsibility
- **Efficiency**: Zero-copy operations where possible

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
Additionally, most libraries require significant codebase changes to support WebAssembly,
whereas yawc maintains compatibility across platforms without forcing developers to rewrite their code.
This library aims to provide all these critical features with an ergonomic interface
that makes WebSocket development straightforward and efficient across native and WASM environments.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
For major changes, please open an issue first to discuss what you would like to change.

## License

This project is licensed under the GNU Lesser General Public License v3.0 (LGPL-3.0).
Under the terms of this license, you may use, modify, and distribute the code as part of a larger work without requiring the entire work to be licensed under the LGPL.
However, any modifications to the library itself must be made available under the LGPL.
For more details, see ([LICENSE](LICENSE) or https://www.gnu.org/licenses/lgpl-3.0.en.html)

## About us

Infinite Field is a high-frequency trading firm. We build ultra-low-latency systems for execution at scale. Performance is everything.

We prioritize practical solutions over theory. If something works and delivers results, that’s what matters. Performance is always the goal, and every piece of code is written with efficiency and longevity in mind.

If you specialize in performance-critical software, understand systems down to the bare metal, and know how to optimize x64 assembly, we’d love to hear from you.

[Explore career opportunities](https://job-boards.eu.greenhouse.io/infinitefield)

## Dev

#### How to run autobahn tests.

The tests require you to have docker started and install deno.

The tests will generate reports with further information on `./autobahn/reports/client/index.html` and `./autobahn/reports/servers/index.html`

Client:

```
deno -A ./autobahn/client-test.js
```

Server:

```
deno -A ./autobahn/server-test.js
```

When testing the server, it will produce a lot of logs stating that clients have connected and disconnected.

This is expected, as the fuzzing client will setup different connections to fuzz the server. This means means that it's working correctly.

## Acknowledgments

Special thanks to:

- The Tungstenite project for inspiration on close codes
- The fastwebsockets project which served as inspiration and source for many implementations
- The Autobahn test suite for protocol compliance verification
