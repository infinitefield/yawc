# yawc

A WebSocket library for Rust with client and server APIs, compression, and manual frame streaming.

[![Crates.io](https://img.shields.io/crates/v/yawc.svg)](https://crates.io/crates/yawc)
[![Documentation](https://docs.rs/yawc/badge.svg)](https://docs.rs/yawc)
[![License](https://img.shields.io/badge/license-MPL%202.0-blue.svg)](LICENSE)
[![Rust Version](https://img.shields.io/badge/rust-1.82%2B-blue.svg)](https://www.rust-lang.org)

## Contents

- [Features](#features)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Examples and guides](#examples-and-guides)
- [Benchmarks](#benchmarks)
- [Development](#development)
- [License](#license)
- [About](#about)
- [Acknowledgments](#acknowledgments)

## Features

- RFC 6455 client and server support, with TLS through `rustls` by default.
- Permessage-deflate compression (RFC 7692), including context takeover controls.
- `WebSocket` for automatic control frames and fragment assembly; `Streaming` for manual frame handling and incremental compression.
- Configurable message limits, fragmentation, backpressure, and UTF-8 validation.
- Browser WebAssembly support. Optional HTTP/2 (RFC 8441), SOCKS5 proxy, Axum, and reqwest integrations.
- `futures::Stream` and `futures::Sink` implementations for async applications.

## Quick start

Add these dependencies to `Cargo.toml`:

```toml
[dependencies]
yawc = "0.4"
futures = { version = "0.3", default-features = false, features = ["std"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Connect to a WebSocket server, send a text frame, and read a reply. Replace the example URL with your server's URL.

```rust
use futures::SinkExt;
use yawc::{Frame, OpCode, Result, WebSocket};

#[tokio::main]
async fn main() -> Result<()> {
    let mut ws = WebSocket::connect("wss://example.com/socket".parse()?).await?;
    ws.send(Frame::text("Hello")).await?;

    let reply = ws.next_frame().await?;
    if reply.opcode() == OpCode::Text {
        println!("{}", reply.as_str());
    }
    Ok(())
}
```

For a server, see the [Hyper echo server](https://github.com/infinitefield/yawc/blob/master/examples/echo_server.rs) or [Axum server](https://github.com/infinitefield/yawc/blob/master/examples/axum.rs).

## Configuration

Use `Options` on either side of a connection. Compression uses the pure Rust deflate backend by default; the `zlib` feature is needed only for window-bit controls.

```rust
use yawc::{CompressionLevel, Options};

let options = Options::default()
    .with_compression_level(CompressionLevel::fast())
    .server_no_context_takeover()
    .client_no_context_takeover()
    .with_max_fragment_size(64 * 1024)
    .with_backpressure_boundary(128 * 1024);
```

`WebSocket` reassembles incoming fragments. Convert it with `ws.into_streaming()` to send and receive individual fragments or compress a message incrementally. See the [streaming example](https://github.com/infinitefield/yawc/blob/master/examples/streaming.rs). The [SOCKS5 example](https://github.com/infinitefield/yawc/blob/master/examples/socks5.rs) shows proxy setup.

Optional Cargo features:

| Feature | Purpose |
|---|---|
| `axum` | Axum server integration |
| `reqwest` | reqwest HTTP client integration |
| `http2` | WebSocket extended CONNECT over HTTP/2 |
| `simd` | SIMD UTF-8 validation |
| `zlib` | Compression window-bit controls |
| `rustls-ring`, `rustls-aws-lc-rs` | Select the fallback TLS crypto provider |

## Examples and guides

- [Runnable examples](https://github.com/infinitefield/yawc/tree/master/examples), including HTTP/2, Axum, SOCKS5, streaming, and custom DNS.
- [API documentation](https://docs.rs/yawc).
- [Upgrade guide](https://github.com/infinitefield/yawc/blob/master/UPGRADE_GUIDE.md) for earlier yawc releases.
- [Migration guide](https://github.com/infinitefield/yawc/blob/master/MIGRATION.md) for tokio-tungstenite users.

## Benchmarks

On 2026-10-02, a matched run completed 210 validated samples over Unix sockets. Each server used one thread; four client workers ran on separate physical cores. Values are median messages per second from five randomized repetitions, with 0.5 seconds of warmup and 2 seconds of measurement per sample. All responses were checked. Higher is better.

yawc used a 128 KiB read buffer, a 64 KiB write backpressure threshold, and batches of up to 32 ready messages. tokio-tungstenite used its default adapter for window 1 and its batched adapter for window 16. fastwebsockets and Beast flushed each response; uWebSockets batched writes internally. The tuned buffers increase memory use per connection.

### Echo

| Binary workload | yawc tuned | tokio-tungstenite | fastwebsockets | uWebSockets | Boost.Beast |
|---|---:|---:|---:|---:|---:|
| 20 B, 64 connections, window 1 | 299,871 | 218,543 | 319,369 | 373,675 | 236,503 |
| 1 KiB, 16 connections, window 1 | 269,074 | 205,838 | 282,914 | 323,312 | 223,912 |
| 1 KiB, 16 connections, window 16 | 1,838,982 | 1,466,336 | 566,137 | 2,292,540 | 276,305 |
| 1 KiB in two fragments, 16 connections, window 16 | 1,527,572 | 1,182,136 | 506,162 | 2,200,804 | 252,129 |

### Telemetry aggregation

Each binary request carries a sequence number and a batch of 32-bit readings. The server sums the readings, updates a per-connection total, and returns a 24-byte acknowledgement containing the sequence, total, and message count. The client validates every acknowledgement.

| Binary workload | yawc tuned | tokio-tungstenite | fastwebsockets | uWebSockets | Boost.Beast |
|---|---:|---:|---:|---:|---:|
| 1 KiB, 16 connections, window 1 | 272,277 | 207,178 | 289,386 | 313,142 | 219,811 |
| 1 KiB, 16 connections, window 16 | 2,086,343 | 1,636,646 | 607,637 | 2,096,138 | 275,843 |
| 20 B, 16 connections, window 16 | 2,681,737 | 2,026,108 | 719,589 | 4,569,648 | 404,668 |

### Beast thread scaling

Separate runs used one or four Asio threads on one or four physical server cores,
with 16 connections and the same client workers and timings as above. Values are
median messages per second from five repetitions. The comparison tables above
use one server thread for every library.

| Binary workload | Beast, 1 thread | Beast, 4 threads |
|---|---:|---:|
| Echo, 1 KiB, window 1 | 222,215 | 616,851 |
| Echo, 1 KiB, window 16 | 265,241 | 635,145 |
| Telemetry, 1 KiB, window 16 | 277,105 | 646,307 |
| Telemetry, 20 B, window 16 | 387,702 | 899,755 |

The allocation benchmark reported zero allocations and reallocations after warmup in all 16 codec, echo, and fragmented echo cases from 20 B to 64 KiB. See the [benchmark instructions](https://github.com/infinitefield/yawc/blob/master/benches/README.md) for the workloads and reproduction commands. These local plaintext results exclude TLS, compression, and application dependencies; hardware and background load affect them.

## Development

Install [`cargo-make`](https://github.com/sagiegurari/cargo-make) and run `cargo make ci` for the repository's format, lint, build, test, documentation, WebAssembly, and Autobahn checks. Individual tasks are defined in [Makefile.toml](https://github.com/infinitefield/yawc/blob/master/Makefile.toml). The full check needs `wasm-pack`, Chrome, Node, Docker, and Deno.

## License

Licensed under [MPL-2.0](LICENSE).

## About

Infinite Field builds performance-sensitive trading systems. [Careers](https://jobs.ashbyhq.com/infinitefield/).

## Acknowledgments

Thanks to the Tungstenite and fastwebsockets projects for ideas, and to the Autobahn test suite for protocol testing.
