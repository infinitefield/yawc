# WebSocket Benchmarks

This directory contains benchmark tools for comparing yawc performance against other WebSocket implementations.

## Prerequisites

### Install OpenSSL (macOS)

```bash
brew install openssl@3
# Ensure it's linked
brew link --force openssl@3
```

## Quick Start

The Makefile will automatically download and build all dependencies:

```bash
cd benches
make
```

This will:

1. Clone uSockets (if not present)
2. Build uSockets library
3. Compile the load_test C client
4. Link everything together

The expected directory structure after setup:

```
yawc/
└── benches/
    ├── Makefile
    ├── load_test.c
    ├── load_test (compiled binary)
    ├── run.js
    └── uSockets/          (auto-cloned)
        ├── src/
        └── *.o (compiled objects)
```

## Manual Setup (Optional)

If you prefer to set up dependencies manually:

```bash
# Setup dependencies only
make setup

# Or manually:
cd benches
git clone https://github.com/uNetworking/uSockets.git
cd uSockets
make WITH_OPENSSL=1
```

## Running Benchmarks

### Quick Start - Automated Setup

The Makefile can automatically clone and build all WebSocket libraries for you:

```bash
cd benches

# Option 1: Build everything and run benchmarks (requires Deno)
make setup-all  # Clones and builds all libraries
make run        # Runs the benchmark suite

# Option 2: Just build the load_test tool
make            # Builds only load_test (for manual testing)
```

The `make setup-all` command will:

1. Build yawc echo_server
2. Clone and build **fastwebsockets**
3. Clone and build **uWebSockets**
4. Clone and build **tokio-tungstenite**

All repositories will be cloned to the parent directory (`../`).

### Manual Setup (Optional)

If you prefer to set up libraries manually:

```bash
# From the parent directory of yawc
cd ..

# Build each library
cd yawc && cargo build --release --example echo_server && cd ..
git clone https://github.com/denoland/fastwebsockets.git && cd fastwebsockets && cargo build --release --example echo_server && cd ..
git clone --recursive https://github.com/uNetworking/uWebSockets.git && cd uWebSockets && WITH_OPENSSL=1 make examples && cd ..
git clone https://github.com/snapview/tokio-tungstenite.git && cd tokio-tungstenite && cargo build --release --example echo-server && cd ..
```

### Benchmark Results

The benchmark suite tests all libraries across different scenarios:

**Libraries tested:**

- **yawc** (this library)
- **fastwebsockets** (Rust)
- **uWebSockets** (C++)
- **tokio-tungstenite** (Rust)

**Test scenarios:**

- Various connection counts (10, 100, 200, 500)
- Various payload sizes (20 bytes, 1KB, 16KB)

Charts will be saved as `<connections>-<bytes>-chart.svg` in the benches directory.

## Manual Testing

You can run the load test manually:

```bash
# ./load_test <connections> <host> <port> <ssl> <delay_ms> <payload_size>
./load_test 100 0.0.0.0 8080 0 0 1024
```

Parameters:

- `connections`: Number of concurrent WebSocket connections
- `host`: Server hostname or IP
- `port`: Server port
- `ssl`: 0 for ws://, 1 for wss://
- `delay_ms`: Delay between messages (0 for no delay)
- `payload_size`: Size of the message payload in bytes

## Cleanup

```bash
make clean
```

This removes compiled object files and the load_test binary.
