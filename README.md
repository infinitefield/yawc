# yawc

Fast, secure, and RFC-compliant WebSocket implementation for Rust with advanced compression support.

yawc combines a high-level WebSocket API with low-level streaming control and compression support, for applications ranging from chat services to data streaming systems.

[![Crates.io](https://img.shields.io/crates/v/yawc.svg)](https://crates.io/crates/yawc)
[![Documentation](https://docs.rs/yawc/badge.svg)](https://docs.rs/yawc)
[![License](https://img.shields.io/badge/license-MPL%202.0-blue.svg)](LICENSE)
[![Rust Version](https://img.shields.io/badge/rust-1.82%2B-blue.svg)](https://www.rust-lang.org)

## Why yawc?

yawc provides a flexible WebSocket API for Rust:

### Unique Features

**Production-grade performance**

- Zero-copy frame processing where possible
- SIMD-optimized masking operations
- Compact memory layout (16-byte frame state vs 24+ bytes in other libraries)
- Powers 24/7 high-frequency trading systems

**Streaming compression support**

- Compress data incrementally without buffering entire messages in memory
- Partial flush support for real-time compression
- Memory-efficient handling of multi-GB payloads

**Dual-level API design**

- **High-level [`WebSocket`](https://docs.rs/yawc/latest/yawc/struct.WebSocket.html)**: Automatic fragment assembly and compression, with optional UTF-8 validation
- **Low-level `Streaming`**: Manual fragment control, streaming compression, direct frame access
- Seamlessly convert between both as needed: `ws.into_streaming()`

## Features

- **Full RFC 6455 Compliance**: Complete implementation of the WebSocket protocol
- **Secure by Default**: Built-in TLS support with `rustls`
- **Advanced Compression**: Support for permessage-deflate (RFC 7692) with streaming compression and context takeover control
- **Zero-Copy Design**: Efficient frame processing with minimal allocations
- **Streaming API**: Low-level API for manual fragment control, memory-efficient processing of large messages, and fine-grained compression control
- **Automatic Frame Management**: Handles control frames and fragmentation automatically, or use `Streaming` for manual control
- **Flow Control**: Configurable backpressure boundaries and automatic fragmentation for large messages
- **Autobahn Test Suite**: Passes all test cases for both client and server modes
- **WebAssembly Support**: Works seamlessly in WASM environments for browser-based applications (both text and binary modes supported)
- **HTTP/2 (RFC 8441)**: Optional extended CONNECT handshake for client and server, behind the `http2` feature
- **SOCKS5 Proxy**: Client connections can be dialled through a SOCKS5 proxy (RFC 1928), with username/password authentication

## About compression

yawc supports WebSocket compression through the [Options](https://docs.rs/yawc/latest/yawc/struct.Options.html) struct with the permessage-deflate extension (RFC 7692).

### Basic Compression

Run the Hyper server example below before running this client.

```rust
use yawc::{CompressionLevel, Options, Result, WebSocket};

#[tokio::main]
async fn main() -> Result<()> {
    let client = WebSocket::connect("ws://127.0.0.1:9002".parse()?)
        .with_options(Options::default().with_compression_level(CompressionLevel::fast()))
        .await?;
    drop(client);
    Ok(())
}
```

### Advanced Compression Features

- **Streaming compression**: Compress large messages incrementally without buffering (available via `Streaming` API)
- **Context takeover control**: Reset compression state between messages for consistent memory usage
- **Configurable compression levels**: Balance between compression ratio and CPU usage
- **Window size control**: Fine-tune memory vs compression tradeoff (requires `zlib` feature)

```rust
// Example: Memory-optimized compression for long-lived connections
use yawc::{CompressionLevel, Options};

fn main() {
    let _options = Options::default()
        .with_compression_level(CompressionLevel::fast())
        .server_no_context_takeover()  // Reset context after each message
        .client_no_context_takeover(); // Prevent client-side memory growth
}
```

The `zlib` feature is NOT mandatory to enable compression. `zlib` is only required for the [window bits](https://docs.rs/yawc/latest/yawc/struct.Options.html#method.with_client_max_window_bits) configuration parameters.
By default yawc uses [flate2](https://docs.rs/flate2/) with the miniz_oxide backend - a pure Rust implementation of deflate.

## Upgrading or Migrating?

- **Migrating from yawc 0.2.x to 0.3.x?** See the [Upgrade Guide](UPGRADE_GUIDE.md) for the version-specific steps.
- **Migrating from tokio-tungstenite?** See the [Migration Guide](MIGRATION.md) for a comprehensive comparison and migration steps.

## Which crate should I use?

When choosing a WebSocket implementation, many developers default to [tokio-tungstenite](https://github.com/snapview/tokio-tungstenite).
As the most stable and widely-used crate in the ecosystem, it provides excellent abstraction over the
WebSocket protocol through its [WebSocketStream](https://docs.rs/tokio-tungstenite/latest/tokio_tungstenite/struct.WebSocketStream.html) type,
which allows projects to implement custom protocols via its generic `<S>` parameter.

yawc also accepts custom async transports through [`WebSocket::from_stream`](https://docs.rs/yawc/latest/yawc/struct.WebSocket.html#method.from_stream)
after the WebSocket handshake has completed, and exposes WebSocket frames through
[futures::Stream](https://docs.rs/futures/latest/futures/prelude/trait.Stream.html)
and [futures::Sink](https://docs.rs/futures/latest/futures/prelude/trait.Sink.html) implementations.
Key features include built-in compression support, zero-copy operations where possible, and first-class WebAssembly support for UI development.
Beyond passing comprehensive test suites including Autobahn,
yawc has proven its reliability in production environments powering 24/7 market trading systems.

## Runtime Support

yawc is built on tokio's I/O traits but can work with other async runtimes through simple adapters. While the library uses tokio internally for its codec and I/O operations, you can integrate it with runtimes like `smol`, `async-std`, or others by implementing trait bridges.

See the [`client_smol.rs`](https://github.com/infinitefield/yawc/tree/master/examples/client_smol.rs) example for a complete demonstration of using yawc with the smol runtime via a simple adapter pattern.

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
yawc = "0.4"
futures = { version = "0.3", default-features = false, features = ["std"] }
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "net", "time"] }
```

### Client Example

Run the Hyper server example below in one terminal, then run this client in another.

```rust
use futures::{SinkExt, StreamExt};
use yawc::{frame::Frame, frame::OpCode, Result, WebSocket};

#[tokio::main]
async fn main() -> Result<()> {
    // Connect with default options
    let mut ws = WebSocket::connect("ws://127.0.0.1:9002".parse()?).await?;

    // Send and receive messages
    ws.send(Frame::text("Hello WebSocket!")).await?;

    while let Some(frame) = ws.next().await {
        match frame.opcode() {
            OpCode::Text => println!("Received: {}", frame.as_str()),
            OpCode::Binary => println!("Received binary: {} bytes", frame.payload().len()),
            _ => {} // Handle control frames automatically
        }
        break;
    }

    Ok(())
}
```

```toml
[dependencies]
yawc = { version = "0.4" }
futures = { version = "0.3", default-features = false, features = ["std"] }
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "net"] }
```

### Connecting Through a SOCKS5 Proxy

```rust
use yawc::{Proxy, Result, WebSocket};

#[tokio::main]
async fn main() -> Result<()> {
    // socks5h:// leaves the hostname for the proxy to resolve; socks5:// resolves it
    // locally and sends an address. Credentials are optional.
    let proxy = Proxy::socks5("socks5h://127.0.0.1:1080".parse()?)?;

    let ws = WebSocket::connect("ws://127.0.0.1:9002".parse()?)
        .with_proxy(proxy)
        .await?;

    Ok(())
}
```

Run the Hyper server example, then replace the proxy address with a running SOCKS5 proxy.

TLS runs end to end through the tunnel, so a `wss://` connection is negotiated with the
target and the proxy only ever sees ciphertext.

### Server Example

```rust
use futures::{SinkExt, StreamExt};
use http_body_util::Empty;
use hyper::{
    body::{Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
    Request, Response,
};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use yawc::{CompressionLevel, Options, Result, WebSocket};

async fn handle_upgrade(mut req: Request<Incoming>) -> Result<Response<Empty<Bytes>>> {
    let options = Options::default().with_compression_level(CompressionLevel::fast());
    let (response, upfn) = WebSocket::upgrade_with_options(&mut req, options)?;
    tokio::spawn(async move {
        let Ok(ws) = upfn.await else { return };
        let mut ws = ws.into_streaming();
        while let Some(frame) = ws.next().await {
            if ws.send(frame).await.is_err() {
                break;
            }
        }
    });
    Ok(response)
}

#[tokio::main]
async fn main() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:9002").await?;
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let connection = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service_fn(handle_upgrade))
                .with_upgrades();
            if let Err(error) = connection.await {
                eprintln!("HTTP connection failed: {error}");
            }
        });
    }
}
```

```toml
[dependencies]
yawc = "0.4"
futures = { version = "0.3", default-features = false, features = ["std"] }
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "net"] }
hyper = { version = "1", features = ["http1", "server"] }
hyper-util = { version = "0.1", features = ["tokio"] }
http-body-util = "0.1"
bytes = "1"
```

The [`examples`](https://github.com/infinitefield/yawc/tree/master/examples) directory contains several documented and runnable examples showcasing advanced WebSocket functionality.
You can find a particularly comprehensive example in the [`axum_proxy`](https://github.com/infinitefield/yawc/tree/master/examples/axum_proxy) implementation, which demonstrates:

- Building a WebSocket broadcast server that efficiently relays messages between multiple connected clients
- Creating a reverse proxy that transparently forwards WebSocket connections to upstream servers
- Proper connection lifecycle management and error handling with the Axum web framework
- Advanced usage patterns like connection pooling and message filtering

These examples serve as practical reference implementations for common WebSocket architectural patterns and best practices using yawc.

## Feature Flags

- `reqwest`: Use reqwest as the HTTP client
- `axum`: Enable integration with the Axum web framework
- `http2`: Enable WebSockets over HTTP/2 via extended CONNECT (RFC 8441)
- `simd`: Enable SIMD-accelerated UTF-8 validation
- `zlib`: Enable advanced compression options with zlib (not recommended unless you know what you are doing). Without this option, yawc will use miniz_oxide, a Rust deflate implementation.
- `rustls-ring`: Enable the fallback rustls crypto provider based on `ring`
- `rustls-aws-lc-rs`: Enable the fallback rustls crypto provider based on `aws-lc-rs`

### Axum Server Example

```rust
use axum::{response::IntoResponse, routing::get, Router};
use futures::{SinkExt, StreamExt};
use yawc::{IncomingUpgrade, Options, CompressionLevel};

async fn websocket_handler(ws: IncomingUpgrade) -> impl IntoResponse {
    let options = Options::default()
        .with_compression_level(CompressionLevel::default())
        .with_utf8();

    let (response, ws_future) = ws.upgrade(options).unwrap();

    // Handle the WebSocket connection in a separate task
    tokio::spawn(async move {
        if let Ok(mut ws) = ws_future.await {
            while let Some(frame) = ws.next().await {
                // Echo the received frames back to the client
                let _ = ws.send(frame).await;
            }
        }
    });

    response.into_response()
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/ws", get(websocket_handler));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

To use the Axum integration, add this to your `Cargo.toml`:

The server listens on `127.0.0.1:3000` and accepts WebSocket connections at `ws://127.0.0.1:3000/ws`.

```toml
[dependencies]
yawc = { version = "0.4", features = ["axum"] }
axum = "0.8"
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "net"] }
futures = { version = "0.3", default-features = false, features = ["std"] }
```

## Advanced Features

### Streaming API

For advanced use cases requiring manual control over frame fragmentation, yawc provides a low-level `Streaming` API:

```rust
use futures::{SinkExt, StreamExt};
use yawc::{Frame, OpCode, Result, WebSocket};

#[tokio::main]
async fn main() -> Result<()> {
    // Convert WebSocket to Streaming for manual fragment control
    let ws = WebSocket::connect("ws://127.0.0.1:9002".parse()?).await?;
    let mut streaming = ws.into_streaming();

    // Send a message as multiple fragments manually
    streaming.send(Frame::text("First part").with_fin(false)).await?;
    streaming.send(Frame::continuation(" second part").with_fin(false)).await?;
    streaming.send(Frame::continuation(" final part")).await?;

    // Receive frames without automatic reassembly
    while let Some(frame) = streaming.next().await {
        match frame.opcode() {
            OpCode::Text => println!("Text fragment: FIN={}", frame.is_fin()),
            OpCode::Continuation => println!("Continuation: FIN={}", frame.is_fin()),
            _ => {}
        }
        if frame.is_fin() {
            break;
        }
    }

    Ok(())
}
```

**When to use `Streaming`:**

- Streaming large files directly from/to disk without buffering in memory
- Implementing custom fragmentation strategies for specific protocols
- Processing data incrementally as fragments arrive for real-time applications
- Fine-grained control over compression boundaries and frame timing

**Key differences from `WebSocket`:**

- No automatic fragment reassembly - you receive individual frames
- No automatic fragmentation - you control when messages are split
- Supports streaming compression with partial flushes
- Lower memory usage for large messages
- More control, but requires understanding of WebSocket fragmentation protocol

### Compression Control

Fine-tune compression settings for optimal performance:

```rust
use yawc::{CompressionLevel, Options, Result, WebSocket};

#[tokio::main]
async fn main() -> Result<()> {
    let ws = WebSocket::connect("ws://127.0.0.1:9002".parse()?)
        .with_options(
            Options::default()
                .with_compression_level(CompressionLevel::default())
                .server_no_context_takeover()
                .client_no_context_takeover()
                .with_client_max_window_bits(11),
        )
        .await?;
    drop(ws);
    Ok(())
}
```

Enable the `zlib` feature in `Cargo.toml` to configure window bits.

```toml
[dependencies]
yawc = { version = "0.4", features = ["zlib"] }
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "net"] }
```

**Context Takeover Options:**

The `no_context_takeover` options control how compression state is managed between messages:

- **With context takeover (default)**: The compression dictionary is maintained across messages, providing better compression ratios for similar data but using more memory over time.

- **Without context takeover**: The compression dictionary is reset after each message, trading compression efficiency for consistent memory usage. Ideal for:
  - Long-lived connections where memory growth is a concern
  - Memory-constrained environments (mobile devices, embedded systems)
  - Applications with diverse message content where dictionary reuse provides little benefit

```rust
// Example: Memory-optimized compression for long-lived connections
use yawc::{CompressionLevel, Options};

fn main() {
    let _options = Options::default()
        .with_compression_level(CompressionLevel::fast())
        .server_no_context_takeover()
        .client_no_context_takeover();
}
```

### Automatic Fragmentation and Flow Control

Configure automatic fragmentation and backpressure for large messages:

```rust
use yawc::{Options, Result, WebSocket};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let ws = WebSocket::connect("ws://127.0.0.1:9002".parse()?)
        .with_options(
            Options::default()
                .with_max_fragment_size(64 * 1024)
                .with_backpressure_boundary(128 * 1024)
                .with_fragment_timeout(Duration::from_secs(30)),
        )
        .await?;
    drop(ws);
    Ok(())
}
```

**Fragmentation options:**

- `with_max_fragment_size()`: Automatically split large outgoing messages into fragments
- `with_fragment_timeout()`: Protect against incomplete fragmented messages that never complete
- `with_backpressure_boundary()`: Control memory usage by applying backpressure when write buffer grows

These options are particularly useful for:

- Handling large file uploads/downloads
- Streaming real-time data with bounded memory
- Preventing memory exhaustion from slow consumers

### Split Streams

Split the WebSocket for independent reading and writing:

```rust
use futures::{StreamExt, SinkExt};
use yawc::frame::Frame;
use yawc::{Result, WebSocket};

#[tokio::main]
async fn main() -> Result<()> {
    let ws = WebSocket::connect("ws://127.0.0.1:9002".parse()?).await?;
    let (mut write, mut read) = ws.split();

    tokio::join!(
        async move {
            if let Some(frame) = read.next().await {
                println!("Received: {:?}", frame.opcode());
            }
        },
        async move {
            let _ = write.send(Frame::text("Hello")).await;
        }
    );
    Ok(())
}
```

### Custom Frame Handling

Process frames manually when needed:

```rust
use futures::{SinkExt, StreamExt};
use yawc::{Frame, OpCode, Result, WebSocket};

fn inspect_frame(frame: Frame) {
    match frame.opcode() {
        OpCode::Text => println!("Received text: {}", frame.as_str()),
        OpCode::Ping => println!("Received ping"),
        OpCode::Close => {
            if let Some(code) = frame.close_code() {
                println!("Connection closing with code: {code:?}");
            }
        }
        _ => {}
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut ws = WebSocket::connect("ws://127.0.0.1:9002".parse()?).await?;
    ws.send(Frame::text("Inspect this echoed frame")).await?;
    if let Some(frame) = ws.next().await {
        inspect_frame(frame);
    }
    Ok(())
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

This project is licensed under the Mozilla Public License 2.0 (MPL-2.0).
Under the terms of this license, you may use, modify, and distribute the code as part of a larger work.
Modifications to files covered by the MPL must be made available under the MPL, but you can combine the code with proprietary code in a larger work.
For more details, see ([LICENSE](LICENSE) or https://www.mozilla.org/en-US/MPL/2.0/)

## About us

Infinite Field is a high-frequency trading firm. We build ultra-low-latency systems for execution at scale. Performance is everything.

We prioritize practical solutions over theory. If something works and delivers results, that’s what matters. Performance is always the goal, and every piece of code is written with efficiency and longevity in mind.

If you specialize in performance-critical software, understand systems down to the bare metal, and know how to optimize x64 assembly, we’d love to hear from you.

[Explore career opportunities](https://jobs.ashbyhq.com/infinitefield/)

## Dev

#### Running the checks

The repository ships a [`cargo-make`](https://github.com/sagiegurari/cargo-make)
`Makefile.toml` with the same checks CI runs. Install the runner once:

```
cargo install cargo-make
```

Then run the full pre-PR gate:

```
cargo make ci
```

This runs, in order: format check, clippy (warnings denied), build, tests
(default and `zlib` features), doc build, wasm tests, and the autobahn client
and server suites. The full gate needs `wasm-pack` and `deno` installed, same as
the GitHub runner.

Individual tasks are available too:

```
cargo make format          # format the code in place
cargo make clippy          # lint with -D warnings
cargo make test            # default-feature tests (nextest if installed)
cargo make test-zlib       # tests with the zlib feature
cargo make doc             # build docs, deny doc warnings
cargo make wasm            # wasm tests (headless chrome + node)
cargo make autobahn        # both autobahn suites
```

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
