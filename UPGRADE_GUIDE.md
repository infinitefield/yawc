# Upgrade Guide: yawc 0.3 to 0.3

This guide will help you upgrade your code from yawc 0.3.x to 0.3.0. The 0.3 release includes several breaking changes focused on API simplification and improved architecture.

## Table of Contents

- [Quick Migration Checklist](#quick-migration-checklist)
- [Breaking Changes](#breaking-changes)
  - [FrameView Removed](#1-frameview-removed)
  - [Frame Fields Now Private](#2-frame-fields-now-private)
  - [Feature Flags Changed](#3-feature-flags-changed)
  - [Dependency Updates](#4-dependency-updates)
- [New Features](#new-features)
- [Step-by-Step Migration](#step-by-step-migration)
- [Common Migration Patterns](#common-migration-patterns)
- [Troubleshooting](#troubleshooting)

## Quick Migration Checklist

- [ ] Replace all `FrameView` with `Frame`
- [ ] Update field access to use accessor methods
- [ ] Remove `logging` feature flag (now always enabled)
- [ ] Remove `json` feature flag (add `serde_json` directly if needed)
- [ ] Update `reqwest` to 0.13 if using the `reqwest` feature
- [ ] Update imports (if using `FrameView` specifically)

## Breaking Changes

### 1. FrameView Removed

The `FrameView` type has been removed. All frame operations now use the unified `Frame` type.

#### Before (0.1.x)

```rust
use yawc::frame::FrameView;

let frame = FrameView::text("Hello");
ws.send(frame).await?;

while let Some(frame) = ws.next().await {
    match frame.opcode {
        OpCode::Text => println!("{}", frame.as_str()),
        _ => {}
    }
}
```

#### After (0.3.x)

```rust
use yawc::frame::Frame;

let frame = Frame::text("Hello");
ws.send(frame).await?;

while let Some(frame) = ws.next().await {
    match frame.opcode() {
        OpCode::Text => println!("{}", frame.as_str()),
        _ => {}
    }
}
```

**Changes needed:**

- Replace `FrameView` with `Frame` in all imports and type annotations
- Update field access to use methods (see next section)

### 2. Frame Fields Now Private

Direct field access on `Frame` is no longer possible. Use accessor methods instead.

#### Field Access Migration

| 0.3.x (Old)     | 0.3.x (New)             | Notes                                            |
| --------------- | ----------------------- | ------------------------------------------------ |
| `frame.opcode`  | `frame.opcode()`        | Returns `OpCode`                                 |
| `frame.payload` | `frame.payload()`       | Returns `&Bytes`                                 |
| `frame.fin`     | `frame.is_fin()`        | Returns `bool`                                   |
| N/A             | `frame.is_compressed()` | New accessor for compression flag                |
| N/A             | `frame.into_parts()`    | Returns `(OpCode, bool, Bytes)` - consumes frame |

#### Before (0.1.x)

```rust
let opcode = frame.opcode;
let payload = frame.payload.clone();
let is_final = frame.fin;

// Pattern matching
match frame.opcode {
    OpCode::Text => {
        let text = std::str::from_utf8(&frame.payload)?;
        println!("{}", text);
    }
    _ => {}
}
```

#### After (0.3.x)

```rust
let opcode = frame.opcode();
let payload = frame.payload().clone();
let is_final = frame.is_fin();

// Pattern matching - use method call
match frame.opcode() {
    OpCode::Text => {
        let text = frame.as_str(); // Helper method for text frames
        println!("{}", text);
    }
    _ => {}
}

// Or destructure if you need ownership
let (opcode, is_fin, payload) = frame.into_parts();
match opcode {
    OpCode::Text => {
        let text = std::str::from_utf8(&payload)?;
        println!("{}", text);
    }
    _ => {}
}
```

### 3. Feature Flags Changed

#### Removed Features

##### `logging` Feature (Removed)

The `logging` feature flag has been removed. Logging is now always available through the `log` crate.

**Before (0.1.x):**

```toml
[dependencies]
yawc = { version = "0.1", features = ["logging"] }
```

**After (0.3.x):**

```toml
[dependencies]
yawc = "0.3"
# logging is now always available via the log crate
```

##### `json` Feature (Removed)

The `json` feature flag has been removed. If you need JSON serialization, add `serde_json` directly to your dependencies.

**Before (0.1.x):**

```toml
[dependencies]
yawc = { version = "0.1", features = ["json"] }
```

**After (0.3.x):**

```toml
[dependencies]
yawc = "0.3"
serde_json = "1.0"  # Add directly if needed
serde = { version = "1.0", features = ["derive"] }
```

#### New Features

- **`smol`**: Support for the smol async runtime (see examples/client_smol.rs)

### 4. Dependency Updates

#### reqwest Updated to 0.13

If you're using the `reqwest` feature, update your `reqwest` dependency to 0.13.

**Before (0.1.x):**

```toml
[dependencies]
yawc = { version = "0.1", features = ["reqwest"] }
reqwest = "0.12"
```

**After (0.3.x):**

```toml
[dependencies]
yawc = { version = "0.3", features = ["reqwest"] }
reqwest = "0.13"
```

**Note:** reqwest 0.13 has its own breaking changes. Consult the [reqwest changelog](https://github.com/seanmonstar/reqwest/blob/master/CHANGELOG.md) if you use reqwest directly in your code.

## New Features

While migrating, you may want to take advantage of new features in 0.3:

### Fragment Timeout

Protect against incomplete fragmented messages:

```rust
use yawc::{WebSocket, Options};
use std::time::Duration;

let ws = WebSocket::connect("wss://example.com".parse()?)
    .with_options(
        Options::default()
            .with_fragment_timeout(Duration::from_secs(30))
    )
    .await?;
```

### Frame Builder Methods

Create fragmented messages more easily:

```rust
use yawc::Frame;

// Send a fragmented text message
let first = Frame::text("Hello, ").with_fin(false);
let last = Frame::continuation("World!");

ws.send(first).await?;
ws.send(last).await?;
```

### Improved Frame Helpers

```rust
// Safe string conversion
let text = frame.as_str(); // Returns &str, panics if not UTF-8

// Extract close frame data
if let Some(code) = frame.close_code() {
    println!("Close code: {:?}", code);
}
if let Ok(Some(reason)) = frame.close_reason() {
    println!("Close reason: {}", reason);
}

// Destructure for ownership
let (opcode, is_fin, payload) = frame.into_parts();
```

### Multi-Runtime Support

yawc 0.3 can work with async runtimes other than tokio:

```rust
// See examples/client_smol.rs for a complete example
use smol::net::TcpStream;

// Implement a simple adapter
struct SmolStream(TcpStream);

impl tokio::io::AsyncRead for SmolStream {
    // Bridge implementation
}

impl tokio::io::AsyncWrite for SmolStream {
    // Bridge implementation
}
```

## Step-by-Step Migration

### Step 1: Update Dependencies

Update your `Cargo.toml`:

```toml
[dependencies]
# Before
# yawc = { version = "0.1", features = ["logging", "json", "reqwest"] }

# After
yawc = { version = "0.3", features = ["reqwest"] }
serde_json = "1.0"  # Only if you were using the json feature
reqwest = "0.13"    # Only if you were using the reqwest feature
```

### Step 2: Update Imports

Replace `FrameView` imports with `Frame`:

```rust
// Before
use yawc::frame::FrameView;

// After
use yawc::frame::Frame;
```

### Step 3: Update Frame Construction

Replace `FrameView` constructors with `Frame`:

```rust
// Before
let frame = FrameView::text("Hello");
let frame = FrameView::binary(vec![1, 2, 3]);
let frame = FrameView::ping("");
let frame = FrameView::close(CloseCode::Normal, b"Goodbye");

// After
let frame = Frame::text("Hello");
let frame = Frame::binary(vec![1, 2, 3]);
let frame = Frame::ping("");
let frame = Frame::close(CloseCode::Normal, b"Goodbye");
```

### Step 4: Update Frame Field Access

Replace direct field access with method calls:

**Find and replace patterns:**

- `frame.opcode` → `frame.opcode()`
- `frame.payload` → `frame.payload()` (returns `&Bytes`, clone if needed)
- `frame.fin` → `frame.is_fin()`

**Using your editor:**

In VSCode:

```
Find:    \.opcode(?!\()
Replace: .opcode()

Find:    \.payload(?!\()
Replace: .payload()

Find:    \.fin(?!\()
Replace: .is_fin()
```

### Step 5: Update Pattern Matching

Change direct field access in patterns to method calls:

```rust
// Before
match frame.opcode {
    OpCode::Text => { /* ... */ }
    OpCode::Binary => { /* ... */ }
    _ => {}
}

// After
match frame.opcode() {
    OpCode::Text => { /* ... */ }
    OpCode::Binary => { /* ... */ }
    _ => {}
}
```

### Step 6: Test Thoroughly

Run your test suite and verify:

- [ ] All WebSocket connections work correctly
- [ ] Message sending and receiving functions properly
- [ ] Control frames (ping/pong/close) are handled correctly
- [ ] Compression still works if you're using it
- [ ] Error handling still works as expected

## Common Migration Patterns

### Pattern 1: Simple Echo Server

**Before (0.1.x):**

```rust
use yawc::{WebSocket, frame::FrameView};
use futures::StreamExt;

while let Some(frame) = ws.next().await {
    match frame.opcode {
        OpCode::Text | OpCode::Binary => {
            ws.send(frame).await?;
        }
        _ => {}
    }
}
```

**After (0.3.x):**

```rust
use yawc::{WebSocket, frame::Frame};
use futures::StreamExt;

while let Some(frame) = ws.next().await {
    match frame.opcode() {
        OpCode::Text | OpCode::Binary => {
            ws.send(frame).await?;
        }
        _ => {}
    }
}
```

### Pattern 2: Processing Text Messages

**Before (0.1.x):**

```rust
while let Some(frame) = ws.next().await {
    if frame.opcode == OpCode::Text {
        let text = std::str::from_utf8(&frame.payload)?;
        process_message(text);
    }
}
```

**After (0.3.x):**

```rust
while let Some(frame) = ws.next().await {
    if frame.opcode() == OpCode::Text {
        let text = frame.as_str(); // More convenient helper
        process_message(text);
    }
}
```

### Pattern 3: Creating Custom Frames

**Before (0.1.x):**

```rust
let mut frames = vec![
    FrameView::text("Message 1"),
    FrameView::text("Message 2"),
    FrameView::binary(vec![1, 2, 3]),
];

for frame in frames {
    ws.send(frame).await?;
}
```

**After (0.3.x):**

```rust
let frames = vec![
    Frame::text("Message 1"),
    Frame::text("Message 2"),
    Frame::binary(vec![1, 2, 3]),
];

for frame in frames {
    ws.send(frame).await?;
}
```

### Pattern 4: Handling Close Frames

**Before (0.1.x):**

```rust
if frame.opcode == OpCode::Close {
    if let Some(code) = frame.close_code() {
        println!("Closing with code: {:?}", code);
    }
    if let Ok(Some(reason)) = frame.close_reason() {
        println!("Reason: {}", reason);
    }
}
```

**After (0.3.x):**

```rust
if frame.opcode() == OpCode::Close {
    if let Some(code) = frame.close_code() {
        println!("Closing with code: {:?}", code);
    }
    if let Ok(Some(reason)) = frame.close_reason() {
        println!("Reason: {}", reason);
    }
}
```

### Pattern 5: Destructuring Frames

**New in 0.3.x** - Use `into_parts()` when you need owned data:

```rust
// Before (0.1.x) - had to clone
let opcode = frame.opcode;
let payload = frame.payload.clone();

// After (0.3.x) - can get ownership without cloning
let (opcode, is_fin, payload) = frame.into_parts();
// payload is now owned Bytes, no clone needed
```

## Troubleshooting

### Compiler Error: "no field `opcode` on type `Frame`"

**Cause:** Direct field access is no longer allowed.

**Solution:** Use the accessor method:

```rust
// Error
let op = frame.opcode;

// Fix
let op = frame.opcode();
```

### Compiler Error: "cannot find type `FrameView`"

**Cause:** `FrameView` has been removed.

**Solution:** Replace with `Frame`:

```rust
// Error
use yawc::frame::FrameView;
let frame: FrameView = ...;

// Fix
use yawc::frame::Frame;
let frame: Frame = ...;
```

### Compiler Error: "no method named `from_utf8` found"

**Cause:** Trying to access payload as a field.

**Solution:** Use `payload()` method or `as_str()` helper:

```rust
// Error
let text = std::str::from_utf8(&frame.payload)?;

// Fix 1: Use accessor
let text = std::str::from_utf8(frame.payload())?;

// Fix 2: Use helper (better for text frames)
let text = frame.as_str();
```

### Compiler Error: "cannot move out of `frame.payload`"

**Cause:** `payload()` returns a reference, not owned data.

**Solution:** Either clone or use `into_parts()`:

```rust
// Option 1: Clone
let payload = frame.payload().clone();

// Option 2: Use into_parts() for ownership
let (opcode, is_fin, payload) = frame.into_parts();
```

### Feature `logging` not found

**Cause:** The `logging` feature has been removed.

**Solution:** Remove it from your feature flags:

```toml
# Before
yawc = { version = "0.3", features = ["logging"] }

# After
yawc = "0.3"
```

### Feature `json` not found

**Cause:** The `json` feature has been removed.

**Solution:** Add `serde_json` directly:

```toml
# Before
yawc = { version = "0.3", features = ["json"] }

# After
yawc = "0.3"
serde_json = "1.0"
```

## Performance Considerations

The 0.3 release includes several performance improvements:

- **SIMD optimizations** for frame masking/unmasking (automatic when available)
- **Improved fragment handling** with better memory management
- **Zero-copy improvements** where possible
- **More efficient compression** handling

Your application should see equal or better performance after upgrading. If you notice regressions, please file an issue with benchmarks.

## Need Help?

If you encounter issues not covered in this guide:

1. Check the [CHANGELOG.md](CHANGELOG.md) for complete list of changes
2. Review the [examples](https://github.com/infinitefield/yawc/tree/master/examples) for working code
3. Read the [API documentation](https://docs.rs/yawc)
4. File an issue on [GitHub](https://github.com/infinitefield/yawc/issues)

## Additional Resources

- [CHANGELOG.md](CHANGELOG.md) - Complete list of changes
- [MIGRATION.md](MIGRATION.md) - Migrating from tokio-tungstenite
- [API Documentation](https://docs.rs/yawc) - Complete API reference
- [Examples](https://github.com/infinitefield/yawc/tree/master/examples) - Working example code
