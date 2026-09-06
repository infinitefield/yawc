//! # yawc
//!
//! WebSocket (RFC 6455) with permessage-deflate compression (RFC 7692).
//! Autobahn compliant. Supports WASM targets.
//!
//! # Features
//! - `reqwest`: WebSocket via reqwest HTTP client
//! - `axum`: WebSocket extractor for axum
//! - `http2`: WebSockets over HTTP/2 via extended CONNECT (RFC 8441)
//! - `zlib`: Advanced compression with window size control
//! - `json`: JSON serialization support
//!
//! # Runtime Support
//!
//! yawc is built on tokio's I/O traits but can work with other async runtimes through simple adapters.
//! While the library uses tokio internally for its codec and I/O operations, you can integrate it with
//! runtimes like `smol`, `async-std`, or others by implementing trait bridges between their I/O traits
//! and tokio's `AsyncRead`/`AsyncWrite`.
//!
//! See the [client_smol.rs example](https://github.com/infinitefield/yawc/tree/master/examples/client_smol.rs)
//! for a complete demonstration of using yawc with the smol runtime.
//!
//! # Client Example
//! ```rust
//! use futures::{SinkExt, StreamExt};
//! use yawc::{WebSocket, frame::OpCode};
//!
//! async fn connect() -> yawc::Result<()> {
//!     let mut ws = WebSocket::connect("wss://echo.websocket.org".parse()?).await?;
//!
//!     while let Some(frame) = ws.next().await {
//!         match frame.opcode() {
//!             OpCode::Text | OpCode::Binary => ws.send(frame).await?,
//!             OpCode::Ping => {
//!                 // Pong is sent automatically, but ping is still returned
//!                 // so you can observe it if needed
//!             }
//!             _ => {}
//!         }
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # Protocol Handling
//!
//! yawc automatically handles WebSocket control frames:
//!
//! - **Ping frames**: Automatically responded to with pongs, but still returned to your application
//! - **Pong frames**: Passed through without special handling
//! - **Close frames**: Automatically acknowledged, then returned before closing the connection
//!
//! # Server Example
//! ```rust
//! use http_body_util::Empty;
//! use futures::StreamExt;
//! use hyper::{Request, body::{Incoming, Bytes}, Response};
//! use yawc::WebSocket;
//!
//! async fn upgrade(mut req: Request<Incoming>) -> yawc::Result<Response<Empty<Bytes>>> {
//!     let (response, fut) = WebSocket::upgrade(&mut req)?;
//!
//!     tokio::spawn(async move {
//!         if let Ok(mut ws) = fut.await {
//!             while let Some(frame) = ws.next().await {
//!                 // Process frames
//!             }
//!         }
//!     });
//!
//!     Ok(response)
//! }
//! ```
//!
//! # WebSockets over HTTP/2
//!
//! With the `http2` feature, connections can be carried over a single HTTP/2 stream using
//! the RFC 8441 extended CONNECT handshake instead of the HTTP/1.1 `Upgrade` handshake.
//! Only the handshake changes: framing, masking and `permessage-deflate` are the same.
//!
//! The client stays on HTTP/1.1 unless asked otherwise, so this changes nothing for
//! existing code. Ask for HTTP/2 explicitly when the server is known to support it:
//!
//! ```no_run
//! # #[cfg(feature = "http2")]
//! async fn connect() -> yawc::Result<()> {
//!     use yawc::{HttpVersion, WebSocket};
//!
//!     let ws = WebSocket::connect("wss://example.com/chat".parse()?)
//!         .http_version(HttpVersion::Http2)
//!         .await?;
//!     Ok(())
//! }
//! ```
//!
//! There is deliberately no automatic negotiation. Agreeing on `h2` over ALPN says the
//! peer speaks HTTP/2, not that it implements RFC 8441, and most deployments serve `h2`
//! for ordinary requests while accepting WebSockets over HTTP/1.1 only. Choosing HTTP/2
//! against such a peer fails rather than silently downgrading, so the choice stays with
//! the caller who knows what the server does. To try HTTP/2 and fall back, see
//! [`HttpVersion::Http2`].
//!
//! On the server, [`WebSocket::upgrade`] handles both handshakes already. The one extra
//! step is calling `enable_connect_protocol()` on hyper's HTTP/2 server builder, which is
//! what advertises `SETTINGS_ENABLE_CONNECT_PROTOCOL`. See the `http2_server` example.

#![cfg_attr(docsrs, feature(doc_cfg))]

#[doc(hidden)]
#[cfg(target_arch = "wasm32")]
mod wasm;

#[doc(hidden)]
#[cfg(not(target_arch = "wasm32"))]
mod native;

#[cfg(not(target_arch = "wasm32"))]
mod compression;

pub mod close;
#[cfg(not(target_arch = "wasm32"))]
pub mod codec;
pub mod frame;
#[doc(hidden)]
pub mod mask;
#[cfg(not(target_arch = "wasm32"))]
mod stream;

use thiserror::Error;
#[cfg(target_arch = "wasm32")]
pub use wasm::*;

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

/// Result type for WebSocket operations.
pub type Result<T> = std::result::Result<T, WebSocketError>;

/// Errors that can occur during WebSocket operations.
#[derive(Error, Debug)]
pub enum WebSocketError {
    /// Invalid fragment sequence.
    #[error("Invalid fragment")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidFragment,

    /// Fragmented message timed out.
    #[error("Fragmented message timed out")]
    #[cfg(not(target_arch = "wasm32"))]
    FragmentTimeout,

    /// Payload contains invalid UTF-8.
    #[error("Invalid UTF-8")]
    InvalidUTF8,

    /// Continuation frame without initial frame.
    #[error("Invalid continuation frame")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidContinuationFrame,

    /// HTTP status code not valid for WebSocket upgrade.
    #[error("Invalid status code: {0}")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidStatusCode(u16),

    /// Server responded with a redirect status code.
    #[error("Redirected with status code {status_code} to {location}")]
    #[cfg(not(target_arch = "wasm32"))]
    Redirected {
        /// The HTTP status code (e.g. 301, 302, 307, 308).
        status_code: u16,
        /// The target location from the `Location` header.
        location: String,
    },

    /// Missing or invalid "Upgrade: websocket" header.
    #[error("Invalid upgrade header")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidUpgradeHeader,

    /// Missing or invalid "Connection: upgrade" header.
    #[error("Invalid connection header")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidConnectionHeader,

    /// Connection has been closed.
    #[error("Connection is closed")]
    ConnectionClosed,

    /// Close frame has invalid format.
    #[error("Invalid close frame")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidCloseFrame,

    /// Close frame contains invalid status code.
    #[error("Invalid close code")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidCloseCode,

    /// Reserved bits in frame header are not zero.
    #[error("Reserved bits are not zero")]
    #[cfg(not(target_arch = "wasm32"))]
    ReservedBitsNotZero,

    /// Control frame is fragmented.
    #[error("Control frame must not be fragmented")]
    #[cfg(not(target_arch = "wasm32"))]
    ControlFrameFragmented,

    /// Ping frame exceeds 125 bytes.
    #[error("Ping frame too large")]
    #[cfg(not(target_arch = "wasm32"))]
    PingFrameTooLarge,

    /// Frame payload exceeds configured maximum.
    #[error("Frame too large")]
    #[cfg(not(target_arch = "wasm32"))]
    FrameTooLarge,

    /// Sec-WebSocket-Version is not 13.
    #[error("Sec-Websocket-Version must be 13")]
    #[cfg(not(target_arch = "wasm32"))]
    InvalidSecWebsocketVersion,

    /// Invalid frame opcode.
    #[error("Invalid opcode (byte={0})")]
    InvalidOpCode(u8),

    /// Missing Sec-WebSocket-Key header.
    #[error("Sec-WebSocket-Key header is missing")]
    #[cfg(not(target_arch = "wasm32"))]
    MissingSecWebSocketKey,

    /// URL scheme is not ws:// or wss://.
    #[error("Invalid http scheme")]
    InvalidHttpScheme,

    /// The peer does not support RFC 8441 extended CONNECT.
    ///
    /// A server that has not advertised `SETTINGS_ENABLE_CONNECT_PROTOCOL` rejects the
    /// extended CONNECT stream instead of upgrading it.
    #[error("Peer does not support extended CONNECT (RFC 8441)")]
    #[cfg(all(feature = "http2", not(target_arch = "wasm32")))]
    ExtendedConnectNotSupported,

    /// Received compressed frame but compression not negotiated.
    #[error("Received compressed frame on stream that doesn't support compression")]
    #[cfg(not(target_arch = "wasm32"))]
    CompressionNotSupported,

    /// The SOCKS5 proxy refused or could not complete the connection.
    #[cfg(not(target_arch = "wasm32"))]
    #[error(transparent)]
    Socks5(#[from] Socks5Error),

    /// URL parsing error.
    #[error(transparent)]
    UrlParseError(#[from] url::ParseError),

    /// I/O error.
    #[cfg(not(target_arch = "wasm32"))]
    #[error(transparent)]
    IoError(#[from] std::io::Error),

    /// Hyper HTTP error.
    #[cfg(not(target_arch = "wasm32"))]
    #[error(transparent)]
    HTTPError(#[from] hyper::Error),

    #[cfg(target_arch = "wasm32")]
    #[error("js value: {0:?}")]
    Js(wasm_bindgen::JsValue),

    /// Reqwest error.
    #[error(transparent)]
    #[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
    #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
    Reqwest(#[from] reqwest::Error),
}

impl WebSocketError {
    /// Returns `true` if this is a protocol-level error (RFC 6455 violation).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn is_protocol_error(&self) -> bool {
        matches!(
            self,
            Self::InvalidFragment
                | Self::FragmentTimeout
                | Self::InvalidContinuationFrame
                | Self::InvalidCloseFrame
                | Self::InvalidCloseCode
                | Self::ReservedBitsNotZero
                | Self::ControlFrameFragmented
                | Self::PingFrameTooLarge
                | Self::InvalidOpCode(_)
                | Self::CompressionNotSupported
        )
    }

    /// Returns `true` if this is a handshake error.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn is_handshake_error(&self) -> bool {
        #[cfg(feature = "http2")]
        if matches!(self, Self::ExtendedConnectNotSupported) {
            return true;
        }

        matches!(
            self,
            Self::InvalidStatusCode(_)
                | Self::Redirected { .. }
                | Self::InvalidUpgradeHeader
                | Self::InvalidConnectionHeader
                | Self::InvalidSecWebsocketVersion
                | Self::MissingSecWebSocketKey
                | Self::InvalidHttpScheme
        )
    }

    /// Returns `true` if the connection is closed.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::ConnectionClosed)
    }

    /// Returns `true` if this is a data validation error (invalid UTF-8 or size limit).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn is_data_error(&self) -> bool {
        matches!(self, Self::InvalidUTF8 | Self::FrameTooLarge)
    }

    /// Returns `true` if this wraps an I/O error.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn is_io_error(&self) -> bool {
        matches!(self, Self::IoError(_))
    }

    /// Returns the underlying I/O error, if any.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn as_io_error(&self) -> Option<&std::io::Error> {
        match self {
            Self::IoError(e) => Some(e),
            _ => None,
        }
    }
}
