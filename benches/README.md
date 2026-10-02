# WebSocket benchmarks

The comparison suite runs yawc, tokio-tungstenite 0.30.0,
fastwebsockets 0.10.0, uWebSockets and Boost.Beast. Rust dependencies are locked;
C++ dependencies and compiler versions are recorded by `comparison/build.py`.
Requires Linux, Python 3.12+, Rust, a C++20 compiler, Boost headers and zlib headers.

```sh
python3 benches/comparison/build.py
# Set BENCH_BIND_IP to an address this machine is allowed to listen on.
python3 benches/comparison/run.py --bind-ip "$BENCH_BIND_IP" \
  --output target/comparison/baseline.json
```

Servers bind only the supplied IPv6 address, use an ephemeral port and terminate
after each sample. Select distinct physical cores with `--server-cpu` and
`--client-cpus`; defaults are core IDs 2 and 4,6. Do not use SMT siblings.
Use `--smoke --repeats 1 --seconds 1` to check all five adapters first.
For a local baseline without a network listener, use `--unix` instead of
`--bind-ip`. Unix sockets are created under `target/comparison` and removed
after each sample. Compare results only within the same transport.
Generated measurements belong in `target/comparison/` or the ignored `benches/results/` directory.

Beast uses one Asio worker per available logical CPU by default; the other servers
use one event-loop thread. Compression and TLS are disabled.
TCP runs enable TCP_NODELAY. The common client validates the HTTP upgrade and
every echo or application acknowledgement, uses fresh random masks, and supports
fragmented responses. Text cases enable UTF-8 validation. Handshakes and warmup
are excluded from throughput timing.
Cases cover 20 B to 64 KiB, 1 to 128 connections, binary and text, and windows
of 1 or 16 messages. A window of 16 sends a batch before reading its echoes.
Cases 8 to 10 send each binary message as two separately masked fragments.
Cases 11 and 12 pipeline 20 B and 125 B messages across 16 connections.
Cases 13 to 15 ingest telemetry batches instead of echoing them.
The unbatched Rust adapters and Beast complete a write per response. uWebSockets
batches writes while handling incoming data, which benefits its pipelined case.

### Telemetry ingestion

The telemetry cases model a stateful binary request-response service. A message
starts with an 8-byte little-endian sequence number, followed by 32-bit unsigned
readings. Each server sums every reading, adds the batch to a per-connection
running total, and sends a 24-byte binary acknowledgement containing the
sequence number, total, and number of processed batches. The client checks all
three fields on every response, including when 16 requests are pipelined.
Cases 13 and 14 use 1 KiB requests with windows of 1 and 16; case 15 uses
20-byte requests (a sequence and three readings) with a window of 16. The small
case measures per-message overhead; the 1 KiB cases process 254 readings per request.
This adds parsing, state updates, and
response construction without depending on a database or external service.
Use `--case-index 13 --repeats 1 --seconds 1` for a quick validation run.

Run the workloads in the README tables with this command. Select other CPU IDs if these are not
distinct physical cores on your machine:

```sh
python3 benches/comparison/run.py --unix \
  --libraries yawc-buffered-128k tokio-tungstenite tokio-tungstenite-batched \
    fastwebsockets uWebSockets Boost.Beast \
  --case-index 1 2 5 9 13 14 15 --client-cpus 4 6 8 10 \
  --warmup 0.5 --seconds 2 --repeats 5 \
  --output target/comparison/tuned-and-telemetry.json
```

For batching comparisons, select `yawc-batched`, `yawc-buffered`, or
`tokio-tungstenite-batched` with `--libraries`. These adapters use `feed()` for
up to 32 immediately available messages, then flush. They add no timer delay.
`yawc-buffered` also uses `with_read_buffer_capacity(64 * 1024)` and
`with_backpressure_boundary(64 * 1024)`. Larger buffers use more memory per
connection; the library defaults are unchanged. `send()` still flushes each frame.
`yawc-buffered-128k` and `yawc-buffered-512k` change only the read capacity;
both retain the 64 KiB write boundary.
The runner selects `yawc-buffered-128k` by default.

Use `--baseline-server` with `yawc-batched-before` to compare library changes
under the same batching policy. The saved server must support that adapter.

Beast defaults to all available logical CPUs, including those used by the client.
Each connection uses a strand to serialize its handlers. Override the allocation
with `--beast-threads` and `--beast-cpus`; the report records both.

Refresh Beast's README entries with:

```sh
python3 benches/comparison/run.py --unix --libraries Boost.Beast \
  --client-cpus 4 6 8 10 --case-index 1 2 5 9 13 14 15 \
  --warmup 0.5 --seconds 2 --repeats 5 \
  --output target/comparison/beast-all-threads.json
```

## Throughput settings

These settings tune yawc's per-connection buffers, not the OS socket buffers:
128 KiB initial read capacity and a 64 KiB write backpressure threshold.
The threshold can be exceeded by a frame; it is not a hard memory limit.
The read buffer can also grow. These settings increase memory use per connection.

Pass `throughput_options()` to `WebSocket::upgrade_with_options()` on the server,
then pass the upgraded connection to `echo_batched()` below. Clients can pass the
same options to `WebSocket::connect(url).with_options(...)`, but server-side tuning
is what the comparison measures.

```rust
use futures::{FutureExt, SinkExt};
use yawc::{HttpWebSocket, OpCode, Options, Result};

fn throughput_options() -> Options {
    Options::default()
        .with_utf8()
        .without_compression()
        .with_read_buffer_capacity(128 * 1024)
        .with_backpressure_boundary(64 * 1024)
}

async fn echo_batched(mut ws: HttpWebSocket) -> Result<()> {
    loop {
        let mut frame = ws.next_frame().await?;
        for index in 0..32 {
            match frame.opcode() {
                OpCode::Text | OpCode::Binary => ws.feed(frame).await?,
                OpCode::Close => return ws.close().await,
                _ => {}
            }
            if index == 31 {
                break;
            }
            match ws.next_frame().now_or_never() {
                Some(next) => frame = next?,
                None => break,
            }
        }
        ws.flush().await?;
    }
}
```

The loop flushes when no frame is immediately available or after 32 frames.
It never waits for a batch to fill. Backpressure can flush earlier.
For an outgoing batch already available in your application, call `feed()` for each
frame, then `flush()` once; continue polling incoming frames to handle control traffic.

The high-throughput case sends 16 messages before waiting for echoes, across 16
connections, with 1 KiB payloads. A client that waits for each reply prevents that
batching benefit. Reproduce it with `--libraries yawc-buffered-128k --case-index 5`.
The measurements use Unix sockets without TLS or compression; TCP, TLS, application
work and different hardware can change throughput.

## Measurement details

### Allocations

```sh
cargo bench -p yawc --bench allocations -- --assert-zero
```

Counts allocations and reallocations after 1,024 warmup iterations, including both
peers and the in-memory transport. Cases cover codec roundtrips, echo windows of
1 and 16, and two-fragment messages from 20 B to 64 KiB. Telemetry also covers
20 B and 1 KiB requests with both windows, using the comparison server's reusable
acknowledgement buffer. Payloads are consumed
before the next iteration. Connection setup, buffer growth, retained messages,
TLS and compression are outside this check. Fragment assembly retains its buffer
for reuse; retaining a returned message can require another allocation.

### uWebSockets design

The pinned uWebSockets version delivers complete messages as
[borrowed views](https://github.com/uNetworking/uWebSockets/blob/2cb3a77d89045b9e39ca8c85e37f614d2b3afa2b/src/WebSocketContext.h)
of a shared receive buffer. It batches replies in a
[reusable cork buffer](https://github.com/uNetworking/uWebSockets/blob/2cb3a77d89045b9e39ca8c85e37f614d2b3afa2b/src/AsyncSocket.h)
and uses [vectored writes for large plaintext messages](https://github.com/uNetworking/uWebSockets/blob/2cb3a77d89045b9e39ca8c85e37f614d2b3afa2b/src/WebSocket.h).
Its callbacks consume these views synchronously. yawc returns owned payloads that
can survive subsequent reads and move between tasks.

### Socket measurements

The runner randomizes case order and saves every repetition, binary hashes,
dependency versions, client CPU use, server CPU use including warmup, and sampled
batch RTT percentiles. RTT includes queueing and client work; it is not isolated
server latency. Each worker samples one batch in 64. Percentiles stay per worker.
Check client CPU use before interpreting throughput as a server limit.
Results describe local plaintext echo and telemetry aggregation, not TLS,
compression, handshakes, WAN latency, or multicore scaling. Background load
can affect these measurements.

Preserve the original server before editing yawc, then compare both binaries
in the same randomized run:

```sh
mkdir -p target/comparison/before
cp benches/comparison/target/release/server target/comparison/before/server
cp benches/comparison/target/release/load target/comparison/before/load
# Make changes, rebuild, then run:
python3 benches/comparison/build.py --rust-only
python3 benches/comparison/run.py --bind-ip "$BENCH_BIND_IP" \
  --baseline-server target/comparison/before/server \
  --load-generator target/comparison/before/load \
  --libraries yawc-before yawc --output target/comparison/paired.json
```

Codec, masking, in-memory echo and automatic Pong costs can be measured without sockets.
The Pong cases use a 64-byte duplex buffer to exercise control-frame backpressure.
Mask benchmarks align the base buffer to 64 bytes. `mask_alignment` varies offsets for
20 B, 125 B, 126 B and 1 KiB payloads, including unaligned WebSocket payloads.
Use the same benchmark source for both revisions when comparing these timings.

```sh
taskset -c 2 cargo bench -p yawc --bench performance -- --save-baseline before
# After editing:
taskset -c 2 cargo bench -p yawc --bench performance -- --baseline before
```

Validate the load generator and summarize completed runs:

```sh
python3 -m unittest discover -s benches/comparison -p test_load.py
python3 benches/comparison/summarize.py baseline.json paired.json --output results.md
```

Use `--case-index` to select scenarios by their position in `run.py` and
`--client-cpus` to check whether more client workers change throughput.

The old `load_test.c`, `run.js`, and Makefile setup/run targets are retained for
historical use. Their server settings differ, so use `comparison/run.py` for new
comparisons. The C++ adapters use the APIs demonstrated by the upstream
[Beast echo example](https://github.com/boostorg/beast/blob/develop/example/websocket/server/async/websocket_server_async.cpp)
and [uWebSockets echo example](https://github.com/uNetworking/uWebSockets/blob/2cb3a77d89045b9e39ca8c85e37f614d2b3afa2b/examples/EchoServer.cpp).
