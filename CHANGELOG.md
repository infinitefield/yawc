# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

#### Streaming API

- **New `Streaming` type**: Low-level WebSocket API with manual fragment control
  - Provides direct access to WebSocket frames without automatic reassembly
  - Enables memory-efficient streaming of large messages
  - Supports streaming compression with partial flushes
  - Convert from `WebSocket` using `.into_streaming()`
  - See documentation for `Streaming` type for usage examples

#### Compression Enhancements

- **Streaming compression support**: Compress data incrementally without buffering entire messages
  - New `compress_streaming()` method on `Compressor` for partial flushes
  - Enables real-time compression of large payloads
  - Reduces memory usage for large messages

- **No-context-takeover support for streaming**: Reset compression context after each message in streaming mode
  - Ensures consistent memory usage across long-lived connections
  - Available for both `Compressor` and `Decompressor`

#### Configuration Options

- **`with_max_fragment_size(size)`**: Configure automatic fragmentation of outgoing messages
  - Messages exceeding this size are automatically split into fragments
  - Useful for controlling memory usage and latency

- **`with_backpressure_boundary(size)`**: Set backpressure boundary for write buffer
  - Prevents unbounded memory growth when sending large amounts of data
  - Enables flow control for high-throughput applications

- **Fragment timeout configuration**: Control timeout for incomplete fragmented message assembly
  - Protects against partial messages that never complete
  - Configurable via `with_fragment_timeout(duration)`

### Changed

- **Improved compression handling**: Compression now only applies to complete messages (FIN frames)
  - Fragmented messages handle compression at the message level, not per-fragment
  - More efficient and RFC-compliant behavior

- **WriteHalf simplification**: Removed role handling and compression from WriteHalf
  - Compression is now handled at the WebSocket layer
  - Cleaner separation of concerns

### Fixed

- **Compression context management**: Fixed issues with compression state across multiple messages
- **Autobahn test coverage**: Expanded test cases for better protocol compliance verification

## [0.3.1]

### Changed

- **Simplified WriteHalf**: Removed role parameter from WriteHalf implementation
- **Compression handling moved to WebSocket layer**: Compression now only applies to FIN text/binary frames
  - Fragment-level compression is managed at the message level
  - More efficient and correct implementation

### Fixed

- **Compression state management**: Fixed compression context handling for complete messages

### Documentation

- Updated upgrade guide with compression best practices
- Enhanced README with clearer compression examples

## [0.3.0]

### Added

#### Generic Stream Support

- **Generic `AsyncRead + AsyncWrite` support**: WebSocket now accepts any stream implementing tokio's async traits
  - Enables integration with other runtimes via adapters
  - Better flexibility for custom transport layers
  - Examples: `client_smol.rs`, `echo_server_smol.rs`

#### New Examples

- `examples/client_smol.rs`: Using yawc with the smol runtime
- `examples/echo_server_smol.rs`: Echo server with smol runtime
- `examples/streaming.rs`: Streaming large payloads efficiently
- `examples/auth_client.rs`: Authentication flow with custom headers

#### Configuration & Features

- **Fragment timeout support**: Configurable timeout for incomplete fragmented messages
  - New error: `WebSocketError::FragmentTimeout`
  - Configure via `Options::with_fragment_timeout()`

- **TCP_NODELAY support**: Disable Nagle's algorithm for lower latency
  - Configure via `Options::with_no_delay()`
  - Improves latency for small, frequent messages

- **Custom DNS resolution**: Support for custom DNS resolvers and TCP address override
  - Useful for testing and custom networking scenarios
  - See `examples/custom_dns.rs`

#### Compression Improvements

- **Improved compression context handling**: Better management of deflate compression contexts
  - Fixed edge cases in compression negotiation
  - Better handling of window bits with and without values
  - Improved zlib support with `zlib` feature flag

- **Compression test suite**: Extensive tests for compression edge cases
  - Tests for context takeover behavior
  - Fragmented compression tests
  - Stress tests with various data patterns

### Changed

#### Architecture Improvements

- **Fragment handling moved from ReadHalf to WebSocket**: More intuitive and RFC-compliant
  - Transparent fragmentation at the WebSocket level
  - Better separation of concerns
  - Improved testability

- **Simplified codec layer**: More efficient frame encoding/decoding
  - Proper role-based masking enforcement
  - Better buffer management
  - Reduced allocations

- **WebSocket layering improvements**: Cleaner separation between layers
  - Codec → ReadHalf → WebSocket hierarchy
  - Each layer has well-defined responsibilities
  - Better documentation of data flow

#### Code Quality

- **Removed FrameView dependency**: Unified on `Frame` type
  - Simpler API surface
  - Better performance
  - Less confusion about frame ownership

- **Buffer optimizations**: Reduced buffer allocations and copies
  - Payload stored as `Bytes` instead of `BytesMut`
  - Better use of zero-copy operations
  - Improved memory efficiency

#### WASM Improvements

- **Binary mode support for WASM**: Full support for binary WebSocket messages in WebAssembly
  - Previously only text mode was supported
  - Feature parity with native targets

### Breaking Changes

- **Reqwest updated to 0.13**: If using the `reqwest` feature, update your dependency
- **Removed `json` feature flag**: Add `serde_json` directly to dependencies if needed

### Fixed

- **Close frame handling**: Better validation and propagation of close frames
  - Distinguish between empty and malformed close reasons
  - Proper error propagation in `poll_next_frame`
  - Close frames are now exposed to users for custom handling

- **Compression bugs**: Fixed several issues with permessage-deflate
  - Fixed deflate stream suffix handling
  - Improved sync flush behavior
  - Better context takeover management

- **Windows compatibility**: Various fixes for Windows platform support
- **WASM test compatibility**: Tests properly skip on WASM targets
- **Masking bugs**: Fixed role-based frame masking for client/server

### Development

- **Improved Autobahn testing**: Better test scripts and coverage
  - More comprehensive fuzzing
  - Improved test reporting
  - Environment configuration support

- **Better benchmarking infrastructure**: Enhanced performance testing
  - See `benches/` directory for comparative benchmarks

### Documentation

- Enhanced inline documentation throughout the codebase
- Better examples covering common use cases
- Improved architecture documentation in README
- Migration guides updated

## [0.2.0]

### Breaking Changes

#### API Changes

- **Removed `FrameView` type**: The `FrameView` struct has been removed. All frame operations now use the unified `Frame` type.
  - **Migration**: Replace all `FrameView` usage with `Frame`. The API is mostly compatible.
  - See [UPGRADE_GUIDE.md](UPGRADE_GUIDE.md) for detailed migration steps.

- **Frame fields are now private**: Direct field access on `Frame` is no longer possible.
  - **Migration**: Use accessor methods instead:
    - `frame.opcode()` instead of `frame.opcode`
    - `frame.payload()` instead of `frame.payload`
    - `frame.is_fin()` instead of `frame.fin`
    - `frame.into_parts()` to destructure into `(OpCode, bool, Bytes)`

- **Removed `logging` feature flag**: Logging is now always available via the `log` crate.
  - **Migration**: Remove `logging` from your feature flags. The `log` dependency is now unconditional.

- **Removed `json` feature flag**: JSON support via `serde_json` is no longer a built-in feature.
  - **Migration**: Add `serde_json` directly to your dependencies if needed. The feature didn't provide significant value over direct usage.

#### Dependency Updates

- **Updated `reqwest` to 0.13** (from 0.12)
  - **Migration**: If using the `reqwest` feature, update your `reqwest` dependency to 0.13.x
  - Note: reqwest 0.13 has its own breaking changes; consult their changelog if you use reqwest directly.

### Added

#### Runtime Support

- **Multi-runtime support**: yawc can now work with async runtimes other than tokio (smol, async-std, etc.)
  - Added `smol` feature flag for smol runtime examples
  - Provided adapter pattern for integrating with other runtimes
  - See new examples: `client_smol.rs`, `echo_server_smol.rs`

#### New Examples

- `examples/client_smol.rs`: Complete example of using yawc with the smol runtime
- `examples/echo_server_smol.rs`: Echo server implementation using smol runtime
- `examples/fragmented_messages.rs`: Demonstration of handling fragmented WebSocket messages
- `examples/streaming.rs`: Example of streaming large payloads efficiently
- `examples/auth_client.rs`: Authentication flow example with custom headers

#### Performance & Benchmarking

- **New benchmarking suite** in `benches/` directory
  - Comparative benchmarks against fastwebsockets, uWebSockets, and tokio-tungstenite
  - Load testing tool (`load_test.c`) for stress testing
  - Automated benchmark runner with visualization (`run.js`)
  - See `benches/README.md` for usage instructions

- **SIMD optimizations for masking operations**
  - Improved performance for frame masking/unmasking using SIMD when available
  - Automatic fallback to scalar implementation

#### New Features

- **Fragment timeout support**: Configurable timeout for fragmented message assembly
  - Protects against incomplete fragmented messages that never complete
  - New error: `WebSocketError::FragmentTimeout`

- **Improved Frame API**:
  - `Frame::with_fin(bool)` - Builder method to set FIN bit
  - `Frame::continuation(payload)` - Create continuation frames
  - `frame.as_str()` - Safe conversion to &str for text frames
  - `frame.close_code()` - Extract close code from close frames
  - `frame.close_reason()` - Extract close reason from close frames
  - `frame.into_parts()` - Destructure into `(OpCode, bool, Bytes)`

- **Enhanced documentation**:
  - Added comprehensive `MIGRATION.md` guide for migrating from tokio-tungstenite
  - Expanded API documentation with more examples
  - Added architecture diagrams in README
  - Better inline documentation for complex operations

#### WASM Improvements

- **Binary mode support for WebAssembly**: WASM target now supports both text and binary messages
  - Previously only text (UTF-8) mode was supported
  - Full feature parity with native targets for message types

### Changed

#### Architecture Improvements

- **Refactored native module** into cleaner sub-modules:
  - `native/builder.rs` - Connection builder pattern
  - `native/mod.rs` - Core WebSocket implementation
  - `native/options.rs` - Configuration options
  - `native/split.rs` - Split read/write implementation
  - `native/upgrade.rs` - HTTP upgrade handling
  - Total: ~2,900 lines moved from single file to organized modules

- **Improved layered architecture**:
  - Clearer separation between codec, fragment assembly, and protocol layers
  - Better documentation of data flow through layers
  - See README.md for architecture diagram

- **Fragment handling moved from ReadHalf to WebSocket**:
  - More intuitive API - fragmentation is now transparent at the WebSocket level
  - Better alignment with RFC 6455 specification
  - Improved testability

#### Code Quality

- **Simplified decoder implementation**:
  - More efficient frame parsing
  - Better error messages
  - Reduced allocations

- **Enhanced UTF-8 validation**:
  - Optional SIMD-accelerated UTF-8 validation with `simd` feature
  - Better error reporting for invalid UTF-8 sequences

- **Improved compression handling**:
  - More robust permessage-deflate implementation
  - Better handling of context takeover settings
  - Fixed edge cases in compression negotiation

### Fixed

- **Windows compatibility issues**: Various fixes for Windows platform support
- **WASM test handling**: Properly skip platform-specific tests on WASM targets
- **Close frame validation**: Better validation of close codes and reasons
- **Compression edge cases**: Fixed issues with compressed fragmented messages
- **Connection upgrade robustness**: More reliable HTTP upgrade handshake handling

### Development

- **Added development dependencies**:
  - `console-subscriber` for tokio-console debugging
  - `criterion` for benchmarking
  - `anyhow` for better error handling in examples
  - Additional dependencies for comprehensive testing

- **Improved CI/CD**:
  - Updated GitHub Actions workflows
  - Better test coverage
  - Autobahn test suite integration

### Documentation

- Added `MIGRATION.md` with comprehensive migration guide from tokio-tungstenite
- Enhanced README with:
  - Runtime support section
  - Architecture diagram
  - Performance considerations
  - Better feature flag documentation
- Improved inline documentation throughout the codebase
- Added more examples covering common use cases

### Removed

- `FrameView` type (see Breaking Changes)
- `logging` feature flag (now always enabled via `log` crate)
- `json` feature flag (use `serde_json` directly instead)
- Dead code and unused internal utilities
- Backwards compatibility shims from earlier versions

---

## [0.1.x] - Previous Releases

For changes in 0.1.x releases, please see the git history. The 0.2.0 release represents a significant refactoring and improvement of the codebase.

---

## Migration Guide

For detailed migration instructions from 0.1.x to 0.2.0, see [UPGRADE_GUIDE.md](UPGRADE_GUIDE.md).

For migration from tokio-tungstenite, see [MIGRATION.md](MIGRATION.md).
