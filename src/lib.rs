//! # yawc
//! Implementation of the WebSocket protocol (RFC 6455) and permessage-deflate compression (RFC 7692),
//! offering automatic handling for control frames, message fragmentation, and negotiated compression.
//!
//! The WebSocket implementation is fully compliant with the Autobahn test suite for both server and client configurations.
//!
//! The library supports WebAssembly (WASM) targets, allowing WebSocket connections in browser environments, with a few caveats:
//! frame manipulation is not possible since browsers don't allow sending raw frames (though the FrameView struct is still used),
//! and only text mode (UTF-8 strings) is currently supported. Pull requests are welcome to add binary support.
//!
//! # Features
//! The crate provides several optional features that can be enabled in your `Cargo.toml`:
//!
//! - `reqwest`: Enables WebSocket support using reqwest as the HTTP client. Recommended for client-side
//!   applications that need a higher-level HTTP client interface.
//!
//! - `axum`: Enables WebSocket support for the axum web framework through an extractor. Allows handling
//!   WebSocket connections in axum route handlers.
//!
//! - `zlib`: Enables advanced compression options using zlib, including window size control. This allows
//!   fine-tuning of the compression level and memory usage through `client_max_window_bits` and
//!   `server_max_window_bits` options.
//!
//! - `logging`: Enables debug logging for connection negotiation and frame processing using the `log` crate.
//!   Useful for debugging WebSocket connections.
//!
//! - `json`: Enables serialization of JSON data. Useful for handling JSON payloads in WebSocket messages.
//!
//! ## Usage Example
//! ```toml
//! [dependencies]
//! yawc = { version = "0.1", features = ["axum"] }
//! ```
//!
//! # Compression Support
//! The crate implements the permessage-deflate WebSocket extension (RFC 7692) with configurable options:
//!
//! - Context takeover control for both client and server
//! - Compression level adjustment (0-9)
//! - Window size configuration (with `zlib` feature)
//! - Memory usage optimization through no-context-takeover options
//!
//! # Client Example
//! ```rust
//! use tokio::net::TcpStream;
//! use futures::{SinkExt, StreamExt};
//! use yawc::{WebSocket, frame::OpCode};
//!
//! async fn client_connect(client: reqwest::Client) -> yawc::Result<()> {
//!     let mut ws = WebSocket::connect("wss://echo.websocket.org".parse()?).await?;
//!
//!     while let Some(frame) = ws.next().await {
//!         match frame.opcode {
//!             OpCode::Text | OpCode::Binary => {
//!                 ws.send(frame).await?;
//!             }
//!             _ => {}
//!         }
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # Server Example
//! ```rust
//! use http_body_util::Empty;
//! use futures::StreamExt;
//! use hyper::{Request, body::{Incoming, Bytes}, Response};
//! use yawc::{WebSocket};
//!
//! async fn server_upgrade(
//!     mut req: Request<Incoming>,
//! ) -> yawc::Result<Response<Empty<Bytes>>> {
//!     let (response, fut) = WebSocket::upgrade(&mut req)?;
//!
//!     tokio::spawn(async move {
//!         if let Ok(mut ws) = fut.await {
//!             while let Some(frame) = ws.next().await {
//!               // Process frames from the client here
//!             }
//!         }
//!     });
//!
//!     Ok(response)
//! }
//! ```
//!
//! # Memory Safety
//! The crate implements several safety measures:
//! - Maximum payload size limits (configurable, default 2MB)
//! - Automatic handling of control frames
//! - Optional UTF-8 validation for text frames
//! - Protection against memory exhaustion attacks
//!
//! # Performance Considerations
//! - Compression can be tuned for either memory usage or compression ratio
//! - Context takeover settings allow memory-CPU tradeoffs
//! - Zero-copy frame processing where possible
//! - Efficient handling of fragmented messages

#![cfg_attr(docsrs, feature(doc_cfg))]

#[doc(hidden)]
#[cfg(target_arch = "wasm32")]
mod wasm;

#[doc(hidden)]
#[cfg(not(target_arch = "wasm32"))]
mod native;

#[cfg(not(target_arch = "wasm32"))]
mod compression;

#[cfg(not(target_arch = "wasm32"))]
pub mod close;
#[cfg(not(target_arch = "wasm32"))]
pub mod codec;
#[cfg(not(target_arch = "wasm32"))]
pub mod frame;
#[cfg(not(target_arch = "wasm32"))]
mod mask;
#[cfg(not(target_arch = "wasm32"))]
mod stream;

use thiserror::Error;
#[cfg(target_arch = "wasm32")]
pub use wasm::*;

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

/// A result type for WebSocket operations, using `WebSocketError` as the error type.
///
/// This type alias simplifies function signatures within the WebSocket module by providing a
/// standard result type for operations that may return a `WebSocketError`.
pub type Result<T> = std::result::Result<T, WebSocketError>;

/// Represents errors that can occur during WebSocket operations.
///
/// This enum encompasses all possible error conditions that may arise when working with WebSocket connections,
/// including protocol violations, connection issues, and data validation errors. The errors are broadly
/// categorized into:
///
/// - Protocol errors (e.g., invalid frames, incorrect sequence of operations)
/// - Data validation errors (e.g., invalid UTF-8, oversized payloads)
/// - HTTP/Connection errors (e.g., header issues, connection closure)
/// - I/O and system-level errors
///
/// Each variant includes detailed documentation about the specific error condition and when it might occur.
#[derive(Error, Debug)]
pub enum WebSocketError {
    /// Occurs when receiving a WebSocket fragment that violates the protocol specification,
    /// such as receiving a new fragment before completing the previous one.
    #[error("Invalid fragment")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidFragment,

    /// Indicates that a text frame or close frame reason contains invalid UTF-8 data.
    /// According to RFC 6455, all text payloads must be valid UTF-8.
    #[error("Invalid UTF-8")]
    InvalidUTF8,

    /// Occurs when receiving a continuation frame without a preceding initial frame,
    /// or when the continuation sequence is otherwise invalid according to RFC 6455.
    #[error("Invalid continuation frame")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidContinuationFrame,

    /// Returned when receiving an HTTP status code that is not valid for WebSocket handshake.
    /// Only certain status codes (like 101 for successful upgrade) are valid.
    #[error("Invalid status code: {0}")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidStatusCode(u16),

    /// Indicates that the HTTP "Upgrade" header is either missing or does not contain
    /// the required "websocket" value during connection handshake.
    #[error("Invalid upgrade header")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidUpgradeHeader,

    /// Indicates that the HTTP "Connection" header is either missing or does not contain
    /// the required "upgrade" value during connection handshake.
    #[error("Invalid connection header")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidConnectionHeader,

    /// Returned when attempting to perform operations on a closed WebSocket connection.
    /// Once a connection is closed, no further communication is possible.
    #[error("Connection is closed")]
    ConnectionClosed,

    /// Indicates that a received close frame has an invalid format, such as
    /// containing a payload of 1 byte (close frames must be either empty or ≥2 bytes).
    #[error("Invalid close frame")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidCloseFrame,

    /// Occurs when a close frame contains a status code that is not valid according to
    /// RFC 6455 (e.g., using reserved codes or codes in invalid ranges).
    #[error("Invalid close code")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidCloseCode,

    /// Indicates that reserved bits in the WebSocket frame header are set when they
    /// should be 0 according to the protocol specification.
    #[error("Reserved bits are not zero")]
    #[cfg(not(target_arch = "wasm32"))]
    ReservedBitsNotZero,

    /// Occurs when a control frame (ping, pong, or close) is received with the FIN bit
    /// not set. RFC 6455 requires that control frames must not be fragmented.
    #[error("Control frame must not be fragmented")]
    #[cfg(not(target_arch = "wasm32"))]
    ControlFrameFragmented,

    /// Indicates that a received ping frame exceeds the maximum allowed size of 125 bytes
    /// as specified in RFC 6455.
    #[error("Ping frame too large")]
    #[cfg(not(target_arch = "wasm32"))]
    PingFrameTooLarge,

    /// Occurs when a received frame's payload length exceeds the maximum configured size.
    /// This helps prevent memory exhaustion attacks.
    #[error("Frame too large")]
    #[cfg(not(target_arch = "wasm32"))]
    FrameTooLarge,

    /// Returned when the "Sec-WebSocket-Version" header is not set to 13 during handshake.
    /// RFC 6455 requires version 13 for modern WebSocket connections.
    #[error("Sec-Websocket-Version must be 13")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidSecWebsocketVersion,

    /// Indicates receipt of a frame with an invalid opcode value. RFC 6455 defines a specific
    /// set of valid opcodes (0x0 through 0xF).
    #[error("Invalid opcode (byte={0})")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidOpCode(u8),

    /// Occurs during handshake when the required "Sec-WebSocket-Key" header is missing from
    /// the client request.
    #[error("Sec-WebSocket-Key header is missing")]
    #[cfg(not(target_arch = "wasm32"))]
    MissingSecWebSocketKey,

    /// Returned when attempting to establish a WebSocket connection with an invalid URL scheme.
    /// Only "ws://" and "wss://" schemes are valid.
    #[error("Invalid http scheme")]
    InvalidHttpScheme,

    /// Occurs when receiving a compressed frame on a connection where compression was not
    /// negotiated during the handshake.
    #[error("Received compressed frame on stream that doesn't support compression")]
    #[cfg(not(target_arch = "wasm32"))]
    CompressionNotSupported,

    /// Wraps errors from URL parsing that may occur when processing WebSocket URLs.
    #[error(transparent)]
    UrlParseError(#[from] url::ParseError),

    /// Wraps standard I/O errors that may occur during WebSocket communication,
    /// such as connection resets or network timeouts.
    #[cfg(not(target_arch = "wasm32"))]
    #[error(transparent)]
    IoError(#[from] std::io::Error),

    /// Wraps errors from the hyper HTTP library that may occur during the WebSocket
    /// handshake process or connection upgrade.
    #[cfg(not(target_arch = "wasm32"))]
    #[error(transparent)]
    HTTPError(#[from] hyper::Error),

    #[cfg(target_arch = "wasm32")]
    #[error("js value: {0:?}")]
    Js(wasm_bindgen::JsValue),

    /// Wraps errors from the reqwest client library that may occur when using
    /// reqwest for WebSocket connections.
    #[error(transparent)]
    #[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
    #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
    Reqwest(#[from] reqwest::Error),

    /// Occurs when serialization of JSON data fails.
    /// Only available when the `json` feature is enabled.
    #[cfg(all(feature = "json", not(target_arch = "wasm32")))]
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
