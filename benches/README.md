# WebSocket Benchmarks

This directory contains benchmark tools for comparing yawc performance against other WebSocket implementations.

Benchmarks are very undeterministic given that most of the overhead happens in tokio and system calls.

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

### Benchmark Results

The benchmark suite tests all libraries across different scenarios:

**Libraries tested:**

- **yawc** (this crate)
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
