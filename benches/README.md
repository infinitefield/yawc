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
Saved measurements and the comparison report are in [results](results/README.md).

Each server runs one event-loop thread with compression disabled and no TLS.
TCP runs enable TCP_NODELAY. The common client validates the HTTP upgrade and every echoed byte,
uses fresh random masks, and supports fragmented responses. Text cases enable
UTF-8 validation. Handshakes and warmup are excluded from throughput timing.
Cases cover 20 B to 64 KiB, 1 to 128 connections, binary and text, and windows
of 1 or 16 messages. A window of 16 sends a batch before reading its echoes.
Rust adapters and Beast complete a write per echoed message. uWebSockets batches
writes while handling incoming data, which benefits its pipelined case.

For batching comparisons, select `yawc-batched`, `yawc-buffered`, or
`tokio-tungstenite-batched` with `--libraries`. These adapters use `feed()` for
up to 32 immediately available messages, then flush. They add no timer delay.
`yawc-buffered` also uses `with_read_buffer_capacity(64 * 1024)` and
`with_backpressure_boundary(64 * 1024)`. Larger buffers use more memory per
connection; the library defaults are unchanged. `send()` still flushes each frame.
`yawc-buffered-128k` and `yawc-buffered-512k` change only the read capacity;
both retain the 64 KiB write boundary.

Use `--baseline-server` with `yawc-batched-before` to compare library changes
under the same batching policy. The saved server must support that adapter.

The runner randomizes case order and saves every repetition, binary hashes,
dependency versions, client CPU use, server CPU use including warmup, and sampled
batch RTT percentiles. RTT includes queueing and client work; it is not isolated
server latency. Each worker samples one batch in 64. Percentiles stay per worker.
Check client CPU use before interpreting throughput as a server limit.
Results describe local plaintext echo, not TLS, compression, handshakes, WAN
latency, or multicore scaling. Background load can affect these measurements.

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

Codec, masking and in-memory echo costs can be measured without sockets:

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
