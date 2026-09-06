//! Native WebSocket implementation for Tokio runtime.
//!
//! # Architecture Overview
//!
//! This module provides the core WebSocket implementation with two complementary APIs:
//!
//! - **[`WebSocket`]**: High-level API with automatic fragment assembly, compression, and protocol handling
//! - **[`Streaming`]**: Low-level API for manual fragment control and streaming compression
//!
//! ## WebSocket Layer Responsibilities
//!
//! The [`WebSocket`] type provides automatic handling of:
//!
//! - **Fragment assembly**: Automatically reassembles fragmented messages into complete payloads
//! - **Message compression**: Applies permessage-deflate (RFC 7692) decompression to complete messages
//! - **UTF-8 validation**: Validates text frames contain valid UTF-8 (when enabled)
//! - **Protocol control**: Handles Ping/Pong and Close frames automatically
//! - **Automatic fragmentation**: Optionally splits large outgoing messages into fragments
//!
//! ## Streaming Layer Responsibilities
//!
//! The [`Streaming`] type provides direct control over:
//!
//! - **Manual fragments**: Send and receive individual frame fragments without assembly
//! - **Streaming compression**: Compress data incrementally with partial flushes
//! - **Memory efficiency**: Process large messages without buffering entire payloads
//! - **Frame-level access**: Direct access to WebSocket frames for custom protocols
//!
//! ## Complete Architecture Stack
//!
//! ```text
//! ┌────────────────────────────────────────────────┐
//! │  Application Layer                             │
//! └──────────────────┬─────────────────────────────┘
//!                    │
//! ┌──────────────────▼─────────────────────────────┐
//! │  WebSocket Layer (automatic mode)              │
//! │  • Automatic fragment assembly                 │
//! │  • Message-level compression                   │
//! │  • UTF-8 validation for text frames            │
//! │  • Automatic Ping/Pong/Close handling          │
//! │  • Automatic fragmentation (optional)          │
//! └──────────────────┬─────────────────────────────┘
//!                    │
//!          ┌─────────▼─────────┐
//!          │  .into_streaming()│
//!          └─────────┬─────────┘
//!                    │
//! ┌──────────────────▼─────────────────────────────┐
//! │  Streaming Layer (manual mode)                 │
//! │  • Manual fragment control                     │
//! │  • Streaming compression with partial flushes  │
//! │  • Direct frame access                         │
//! │  • Memory-efficient large message handling     │
//! └──────────────────┬─────────────────────────────┘
//!                    │
//! ┌──────────────────▼─────────────────────────────┐
//! │  ReadHalf / WriteHalf                          │
//! │  • Connection state management                 │
//! │  • Buffer coordination                         │
//! └──────────────────┬─────────────────────────────┘
//!                    │
//! ┌──────────────────▼─────────────────────────────┐
//! │  Codec Layer                                   │
//! │  • Frame encoding/decoding                     │
//! │  • Masking/unmasking                           │
//! │  • Header parsing (FIN, RSV, OpCode)           │
//! └──────────────────┬─────────────────────────────┘
//!                    │
//!              Network (TCP/TLS)
//! ```
//!
//! ## Example: WebSocket Mode (Automatic)
//!
//! When receiving a compressed fragmented message with [`WebSocket`]:
//!
//! 1. **Codec**: Decodes 3 individual frames from network bytes
//!    - `Frame(Text, RSV1=1, FIN=0, payload1)`
//!    - `Frame(Continuation, RSV1=0, FIN=0, payload2)`
//!    - `Frame(Continuation, RSV1=0, FIN=1, payload3)`
//!
//! 2. **WebSocket**: Assembles and decompresses automatically
//!    - Concatenates: `payload1 + payload2 + payload3`
//!    - Decompresses the complete message
//!    - Validates UTF-8 for text frames
//!    - Returns: `Frame(Text, payload="decompressed data")`
//!
//! ## Example: Streaming Mode (Manual)
//!
//! When receiving the same message with [`Streaming`]:
//!
//! 1. **Codec**: Decodes individual frames (same as above)
//!
//! 2. **Streaming**: Returns each frame individually
//!    - Application receives: `Frame(Text, RSV1=1, FIN=0, payload1)`
//!    - Application receives: `Frame(Continuation, RSV1=0, FIN=0, payload2)`
//!    - Application receives: `Frame(Continuation, RSV1=0, FIN=1, payload3)`
//!    - Application handles assembly and decompression manually
//!
//! This allows applications to stream-decompress data as fragments arrive,
//! enabling memory-efficient processing of large messages.
//!
//! ## Use Case Selection
//!
//! **Use [`WebSocket`] when:**
//! - You want automatic protocol handling
//! - Messages fit comfortably in memory
//! - You need simple, ergonomic APIs
//!
//! **Use [`Streaming`] when:**
//! - Processing messages larger than available memory (e.g., file transfers)
//! - Implementing custom fragmentation strategies
//! - Building low-latency systems requiring frame-level control
//! - Streaming compression is needed for real-time data

mod builder;
#[cfg(feature = "http2")]
pub mod http2;
mod options;
mod socks5;
mod split;
pub mod streaming;
mod upgrade;

use crate::{codec, compression, frame, streaming::Streaming, Result, WebSocketError};

use {
    bytes::Bytes,
    http_body_util::Empty,
    hyper::{body::Incoming, header, upgrade::Upgraded, Request, Response, StatusCode},
    hyper_util::rt::TokioIo,
    tokio::net::TcpStream,
    tokio_rustls::{rustls::pki_types::ServerName, TlsConnector},
};

use std::{
    borrow::BorrowMut,
    collections::VecDeque,
    future::poll_fn,
    io,
    net::SocketAddr,
    pin::{pin, Pin},
    str::FromStr,
    sync::Arc,
    task::{ready, Context, Poll},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWrite};

use codec::Codec;
use compression::{CompressionConfig, Compressor, Decompressor, WebSocketExtensions};
use futures::{task::AtomicWaker, SinkExt};
use tokio_rustls::rustls::{self, pki_types::TrustAnchor};
use tokio_util::codec::Framed;
use url::Url;

// Re-exports
pub use crate::stream::MaybeTlsStream;
#[cfg(feature = "http2")]
pub use builder::HttpVersion;
pub use builder::{HttpRequest, HttpRequestBuilder, WebSocketBuilder};
pub use frame::{Frame, OpCode};
pub use options::{CompressionLevel, DeflateOptions, Fragmentation, Options};
pub use socks5::{Proxy, Socks5Error};
pub use split::{ReadHalf, WriteHalf};
pub use upgrade::UpgradeFut;

/// Type alias for WebSocket connections established via `connect`.
///
/// This is the default WebSocket type returned by [`WebSocket::connect`],
/// which handles both plain TCP and TLS connections over TCP streams.
pub type TcpWebSocket = WebSocket<MaybeTlsStream<TcpStream>>;

/// Type alias for server-side WebSocket connections from HTTP upgrades or when using reqwest.
///
/// This is the WebSocket type returned by [`WebSocket::upgrade`] and [`UpgradeFut`],
/// which wraps hyper's upgraded HTTP connections.
pub type HttpWebSocket = WebSocket<HttpStream>;

#[cfg(feature = "axum")]
pub use upgrade::IncomingUpgrade;

/// The maximum allowed payload size for reading, set to 1 MiB.
///
/// Frames with a payload size larger than this limit will be rejected to ensure memory safety
/// and prevent excessively large messages from impacting performance.
pub const MAX_PAYLOAD_READ: usize = 1024 * 1024;

/// The maximum allowed read buffer size, set to 2 MiB.
///
/// When the read buffer exceeds this size, it will close the connection
/// to prevent unbounded memory growth from fragmented messages.
pub const MAX_READ_BUFFER: usize = 2 * 1024 * 1024;

/// Type alias for HTTP responses used during WebSocket upgrade.
///
/// This alias represents the HTTP response sent back to clients during a WebSocket handshake.
/// It encapsulates a response with empty body content, which is standard for WebSocket upgrades
/// as the connection transitions from HTTP to the WebSocket protocol after handshake completion.
///
/// Used in conjunction with the [`UpgradeResult`] type to provide the necessary response headers
/// for protocol switching during the WebSocket handshake process.
pub type HttpResponse = Response<Empty<Bytes>>;

/// The result type returned by WebSocket upgrade operations.
///
/// This type represents the result of a server-side WebSocket upgrade attempt, containing:
/// - An HTTP response with the appropriate WebSocket upgrade headers to send to the client
/// - A future that will resolve to a WebSocket connection once the protocol switch is complete
///
/// Both components must be handled for a successful upgrade:
/// 1. Send the HTTP response to the client
/// 2. Await the future to obtain the WebSocket connection
pub type UpgradeResult = Result<(HttpResponse, UpgradeFut)>;

/// Parameters negotiated with the client or the server.
#[derive(Debug, Default, Clone)]
pub(crate) struct Negotiation {
    pub(crate) compression: Option<CompressionConfig>,
    pub(crate) max_payload_read: usize,
    pub(crate) max_read_buffer: usize,
    pub(crate) utf8: bool,
    pub(crate) fragmentation: Option<options::Fragmentation>,
    pub(crate) max_backpressure_write_boundary: Option<usize>,
}

impl Negotiation {
    /// Resolves the negotiated extensions and the local options into the settings a
    /// connection runs with.
    ///
    /// Every handshake path goes through here, so the mapping from [`Options`] lives in
    /// one place.
    ///
    /// # Errors
    ///
    /// Returns an error when the peer negotiated permessage-deflate but this side never
    /// offered it. See [`CompressionConfig::resolve`].
    pub(crate) fn new(
        extensions: Option<WebSocketExtensions>,
        options: &Options,
        role: Role,
    ) -> Result<Self> {
        let compression = CompressionConfig::resolve(
            extensions.as_ref(),
            options.compression.as_ref().map(|deflate| deflate.level),
            role,
        )?;

        // The read buffer holds at least two payloads unless told otherwise.
        let max_payload_read = options.max_payload_read.unwrap_or(MAX_PAYLOAD_READ);
        let max_read_buffer = options.max_read_buffer.unwrap_or(
            options
                .max_payload_read
                .map(|payload_read| payload_read * 2)
                .unwrap_or(MAX_READ_BUFFER),
        );

        Ok(Self {
            compression,
            max_payload_read,
            max_read_buffer,
            utf8: options.check_utf8,
            fragmentation: options.fragmentation.clone(),
            max_backpressure_write_boundary: options.max_backpressure_write_boundary,
        })
    }

    pub(crate) fn compressor(&self) -> Option<Compressor> {
        self.compression.as_ref().map(CompressionConfig::compressor)
    }

    pub(crate) fn decompressor(&self) -> Option<Decompressor> {
        self.compression
            .as_ref()
            .map(CompressionConfig::decompressor)
    }
}

/// The role the WebSocket stream is taking.
///
/// When a server role is taken the frames will not be masked, unlike
/// the client role, in which frames are masked.
#[derive(Copy, Clone, PartialEq)]
pub enum Role {
    Server,
    Client,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server => write!(f, "server"),
            Self::Client => write!(f, "client"),
        }
    }
}

/// Type of context for wake operations in the WebSocket stream.
///
/// Used to distinguish between read and write operations when managing
/// task waking in the WebSocket's asynchronous I/O operations.
#[derive(Clone, Copy)]
enum ContextKind {
    /// Indicates a read operation context
    Read,
    /// Indicates a write operation context
    Write,
}

/// Manages separate wakers for read and write operations on the WebSocket stream.
#[derive(Default)]
struct WakeProxy {
    /// Waker for read operations
    read_waker: AtomicWaker,
    /// Waker for write operations
    write_waker: AtomicWaker,
}

impl futures::task::ArcWake for WakeProxy {
    fn wake_by_ref(this: &Arc<Self>) {
        this.read_waker.wake();
        this.write_waker.wake();
    }
}

impl WakeProxy {
    #[inline]
    fn set_waker(&self, kind: ContextKind, waker: &futures::task::Waker) {
        match kind {
            ContextKind::Read => {
                self.read_waker.register(waker);
            }
            ContextKind::Write => {
                self.write_waker.register(waker);
            }
        }
    }

    #[inline(always)]
    fn with_context<F, R>(self: &Arc<Self>, f: F) -> R
    where
        F: FnOnce(&mut Context<'_>) -> R,
    {
        let waker = futures::task::waker_ref(self);
        let mut cx = Context::from_waker(&waker);
        f(&mut cx)
    }
}

/// An enum representing the underlying WebSocket stream types based on the enabled features.
pub enum HttpStream {
    /// The reqwest-based WebSocket stream, available when the `reqwest` feature is enabled
    #[cfg(feature = "reqwest")]
    Reqwest(reqwest::Upgraded),
    /// The hyper-based WebSocket stream
    Hyper(TokioIo<Upgraded>),
}

impl From<TokioIo<Upgraded>> for HttpStream {
    fn from(value: TokioIo<Upgraded>) -> Self {
        Self::Hyper(value)
    }
}

#[cfg(feature = "reqwest")]
impl From<reqwest::Upgraded> for HttpStream {
    fn from(value: reqwest::Upgraded) -> Self {
        Self::Reqwest(value)
    }
}

impl AsyncRead for HttpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            #[cfg(feature = "reqwest")]
            Self::Reqwest(stream) => pin!(stream).poll_read(cx, buf),
            Self::Hyper(stream) => pin!(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for HttpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::result::Result<usize, io::Error>> {
        match self.get_mut() {
            #[cfg(feature = "reqwest")]
            Self::Reqwest(stream) => pin!(stream).poll_write(cx, buf),
            Self::Hyper(stream) => pin!(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), io::Error>> {
        match self.get_mut() {
            #[cfg(feature = "reqwest")]
            Self::Reqwest(stream) => pin!(stream).poll_flush(cx),
            Self::Hyper(stream) => pin!(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), io::Error>> {
        match self.get_mut() {
            #[cfg(feature = "reqwest")]
            Self::Reqwest(stream) => pin!(stream).poll_shutdown(cx),
            Self::Hyper(stream) => pin!(stream).poll_shutdown(cx),
        }
    }
}

// ================== FragmentLayer ====================

pub(super) struct FragmentationState {
    started: Instant,
    opcode: OpCode,
    is_compressed: bool,
    bytes_read: usize,
    parts: VecDeque<Bytes>,
}

/// Handles fragmentation and defragmentation of WebSocket frames.
///
/// This layer is responsible for:
/// - **Outgoing fragmentation**: Breaking large frames into smaller fragments based on fragment_size
/// - **Incoming defragmentation**: Reassembling fragmented messages received from the peer
///
/// The layer is owned by WebSocket and operates independently for read and write operations.
struct FragmentLayer {
    /// Queue for outgoing fragments (from automatic fragmentation)
    outgoing_fragments: VecDeque<Frame>,
    /// Fragment accumulation for assembling incoming fragmented messages
    incoming_fragment: Option<FragmentationState>,
    /// Maximum fragment size for outgoing messages
    fragment_size: Option<usize>,
    /// Maximum buffer size for incoming fragmented messages
    max_read_buffer: usize,
    /// Timeout for receiving complete fragmented messages
    fragment_timeout: Option<Duration>,
}

impl FragmentLayer {
    /// Creates a new FragmentLayer with the given configuration.
    fn new(
        fragment_size: Option<usize>,
        max_read_buffer: usize,
        fragment_timeout: Option<Duration>,
    ) -> Self {
        Self {
            outgoing_fragments: VecDeque::new(),
            incoming_fragment: None,
            fragment_size,
            max_read_buffer,
            fragment_timeout,
        }
    }

    /// Fragments an outgoing frame if necessary and queues the fragments.
    ///
    /// Panics if the user tries to manually fragment while auto-fragmentation is enabled.
    fn fragment_outgoing(&mut self, frame: Frame) {
        // Check for invalid manual fragmentation with auto-fragmentation enabled
        if !frame.is_fin() && self.fragment_size.is_some() {
            panic!(
                "Fragment the frames yourself or use `fragment_size`, but not both. Use Streaming"
            );
        }

        let max_fragment_size = self.fragment_size.unwrap_or(usize::MAX);
        self.outgoing_fragments
            .extend(frame.into_fragments(max_fragment_size));
    }

    /// Returns the next queued outgoing fragment, if any.
    #[inline(always)]
    fn pop_outgoing_fragment(&mut self) -> Option<Frame> {
        self.outgoing_fragments.pop_front()
    }

    /// Returns true if there are pending outgoing fragments.
    #[inline(always)]
    fn has_outgoing_fragments(&self) -> bool {
        !self.outgoing_fragments.is_empty()
    }

    /// Processes an incoming frame, handling fragmentation and reassembly.
    ///
    /// Returns:
    /// - `Ok(Some(frame))` if a complete frame is ready (either non-fragmented or fully reassembled)
    /// - `Ok(None)` if this is a fragment and we're still waiting for more
    /// - `Err` if there's a protocol violation or timeout
    fn assemble_incoming(&mut self, mut frame: Frame) -> Result<Option<Frame>> {
        use bytes::BufMut;

        #[cfg(test)]
        println!(
            "<<Fragmentation<< OpCode={:?} fin={} len={}",
            frame.opcode(),
            frame.is_fin(),
            frame.payload.len()
        );

        match frame.opcode {
            OpCode::Text | OpCode::Binary => {
                // Check for invalid fragmentation state
                if self.incoming_fragment.is_some() {
                    return Err(WebSocketError::InvalidFragment);
                }

                // Handle fragmented messages
                if !frame.fin {
                    let fragmentation = FragmentationState {
                        started: Instant::now(),
                        opcode: frame.opcode,
                        is_compressed: frame.is_compressed,
                        bytes_read: frame.payload.len(),
                        parts: VecDeque::from([frame.payload]),
                    };
                    self.incoming_fragment = Some(fragmentation);

                    return Ok(None);
                }

                // Non-fragmented message - return as-is
                Ok(Some(frame))
            }
            OpCode::Continuation => {
                let mut fragment = self
                    .incoming_fragment
                    .take()
                    .ok_or_else(|| WebSocketError::InvalidFragment)?;

                fragment.bytes_read += frame.payload.len();

                // Check buffer size
                if fragment.bytes_read >= self.max_read_buffer {
                    return Err(WebSocketError::FrameTooLarge);
                }

                // Check timeout
                if let Some(timeout) = self.fragment_timeout {
                    if fragment.started.elapsed() > timeout {
                        return Err(WebSocketError::FragmentTimeout);
                    }
                }

                fragment.parts.push_back(frame.payload);

                if frame.fin {
                    // Assemble complete message
                    frame.opcode = fragment.opcode;
                    frame.is_compressed = fragment.is_compressed;
                    frame.payload = fragment
                        .parts
                        .into_iter()
                        .fold(
                            bytes::BytesMut::with_capacity(fragment.bytes_read),
                            |mut acc, b| {
                                acc.put(b);
                                acc
                            },
                        )
                        .freeze();

                    Ok(Some(frame))
                } else {
                    self.incoming_fragment = Some(fragment);
                    Ok(None)
                }
            }
            _ => {
                // Control frames pass through unchanged
                Ok(Some(frame))
            }
        }
    }
}

// ================== WebSocket ====================

/// WebSocket stream for both clients and servers.
///
/// The [`WebSocket`] struct manages all aspects of WebSocket communication, handling
/// mandatory frames (close, ping, and pong frames) and protocol compliance checks.
/// It abstracts away details related to framing and compression, which are managed
/// by the underlying [`ReadHalf`] and [`WriteHalf`] structures.
///
/// A [`WebSocket`] instance can be created via high-level functions like [`WebSocket::connect`],
/// or through a custom stream setup with [`WebSocket::handshake`].
///
/// # Automatic Protocol Handling
///
/// The WebSocket automatically handles protocol control frames:
///
/// - **Ping frames**: When a ping frame is received, a pong response is automatically queued for
///   sending. The ping frame is **still returned to the application** via `next()` or the `Stream`
///   trait, allowing you to observe incoming pings if needed.
///
/// - **Pong frames**: Pong frames are passed through to the application without special handling.
///
/// - **Close frames**: When a close frame is received, a close response is automatically sent
///   (if not already closing). The close frame is returned to the application, and subsequent
///   reads will fail with [`WebSocketError::ConnectionClosed`].
///
/// # Compression Behavior
///
/// When compression is enabled (via [`Options::compression`]), the WebSocket automatically
/// compresses and decompresses messages according to RFC 7692 (permessage-deflate):
///
/// - **Outgoing messages**: Only complete (FIN=1) Text or Binary frames are compressed.
///   Fragmented frames (FIN=0 or Continuation frames) are **not** compressed.
///
/// - **Incoming messages**: Compressed messages are automatically decompressed after
///   fragment assembly.
///
/// # Automatic Fragmentation
///
/// When [`Options::with_max_fragment_size`] is configured, the WebSocket will automatically
/// fragment outgoing messages that exceed the specified size limit:
///
/// ```no_run
/// use yawc::{WebSocket, Frame, Options};
/// use futures::SinkExt;
///
/// # async fn example() -> yawc::Result<()> {
/// let options = Options::default()
///     .with_max_fragment_size(64 * 1024); // 64 KiB per frame
///
/// let mut ws = WebSocket::connect("wss://example.com/ws".parse()?)
///     .with_options(options)
///     .await?;
///
/// // This large message will be automatically split into multiple frames
/// let large_message = vec![0u8; 200_000]; // 200 KB
/// ws.send(Frame::binary(large_message)).await?;
/// # Ok(())
/// # }
/// ```
///
/// **Important**: Automatic fragmentation only applies to uncompressed messages. If compression
/// is enabled, the message is compressed first as a single unit, and only the compressed output
/// may be fragmented if it exceeds the size limit.
///
/// # Manual Fragmentation with Streaming
///
/// `WebSocket` automatically handles fragmentation and reassembly. If you need **manual control**
/// over individual fragments (e.g., for streaming large files or custom fragmentation logic),
/// convert the `WebSocket` to a [`Streaming`] connection:
///
/// ```no_run
/// use yawc::{WebSocket, Frame};
/// use futures::SinkExt;
///
/// # async fn example() -> yawc::Result<()> {
/// let ws = WebSocket::connect("wss://example.com/ws".parse()?).await?;
///
/// // Convert to Streaming for manual fragment control
/// let mut streaming = ws.into_streaming();
///
/// // Manually send fragments
/// streaming.send(Frame::text("Hello ").with_fin(false)).await?;
/// streaming.send(Frame::continuation("World").with_fin(false)).await?;
/// streaming.send(Frame::continuation("!")).await?;
/// # Ok(())
/// # }
/// ```
///
/// **Important**: Attempting to send manual fragments (frames with `FIN=0`) through `WebSocket`
/// while automatic fragmentation is enabled will panic. Use [`Streaming`] for manual fragmentation.
///
/// See [`examples/streaming.rs`](https://github.com/infinitefield/yawc/blob/master/examples/streaming.rs)
/// for a complete example of manual fragment control for streaming large files.
///
/// # Connecting
/// To establish a WebSocket connection as a client:
/// ```no_run
/// use tokio::net::TcpStream;
/// use yawc::{WebSocket, frame::OpCode};
/// use futures::StreamExt;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let ws = WebSocket::connect("wss://echo.websocket.org".parse()?).await?;
///     // Use `ws` for WebSocket communication
///     Ok(())
/// }
/// ```
pub struct WebSocket<S> {
    streaming: Streaming<S>,
    check_utf8: bool,
    /// Handles fragmentation and defragmentation of frames
    fragment_layer: FragmentLayer,
}

impl WebSocket<MaybeTlsStream<TcpStream>> {
    /// Establishes a WebSocket connection to the specified `url`.
    ///
    /// This asynchronous function supports both `ws://` (non-secure) and `wss://` (secure) schemes.
    ///
    /// # Parameters
    /// - `url`: The WebSocket URL to connect to.
    ///
    /// # Returns
    /// A `WebSocketBuilder` that can be further configured before establishing the connection.
    ///
    /// # Examples
    /// ```no_run
    /// use yawc::WebSocket;
    ///
    /// #[tokio::main]
    /// async fn main() -> yawc::Result<()> {
    ///    let ws = WebSocket::connect("wss://echo.websocket.org".parse()?).await?;
    ///    Ok(())
    /// }
    /// ```
    pub fn connect(url: Url) -> WebSocketBuilder {
        WebSocketBuilder::new(url)
    }

    pub(crate) async fn connect_priv(
        url: Url,
        tcp_address: Option<SocketAddr>,
        connector: Option<TlsConnector>,
        options: Options,
        builder: HttpRequestBuilder,
    ) -> Result<TcpWebSocket> {
        let host = url.host().expect("hostname").to_string();

        let tcp_stream = if let Some(tcp_address) = tcp_address {
            TcpStream::connect(tcp_address).await?
        } else {
            let port = url.port_or_known_default().expect("port");
            TcpStream::connect(format!("{host}:{port}")).await?
        };

        let _ = tcp_stream.set_nodelay(options.no_delay);

        let stream = match url.scheme() {
            "ws" => MaybeTlsStream::Plain(tcp_stream),
            "wss" => {
                let connector = connector.unwrap_or_else(tls_connector);
                let domain = ServerName::try_from(host)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid dnsname"))?;

                MaybeTlsStream::Tls(connector.connect(domain, tcp_stream).await?)
            }
            _ => return Err(WebSocketError::InvalidHttpScheme),
        };

        WebSocket::handshake_with_request(url, stream, options, builder).await
    }
}

impl<S> WebSocket<S>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    /// Performs a WebSocket handshake over an existing connection.
    ///
    /// This is a lower-level API that allows you to perform a WebSocket handshake
    /// on an already established I/O stream (such as a TcpStream or TLS stream).
    /// For most use cases, prefer using [`WebSocket::connect`] which handles both
    /// connection establishment and handshake automatically.
    ///
    /// # Arguments
    ///
    /// * `url` - The WebSocket URL (used for generating handshake headers)
    /// * `io` - An existing I/O stream that implements AsyncRead + AsyncWrite
    /// * `options` - WebSocket configuration options
    ///
    /// # Example
    ///
    /// ```no_run
    /// use tokio::net::TcpStream;
    /// use yawc::{WebSocket, Options};
    ///
    /// #[tokio::main]
    /// async fn main() -> yawc::Result<()> {
    ///     // Establish your own TCP connection
    ///     let stream = TcpStream::connect("example.com:80").await?;
    ///
    ///     // Parse the WebSocket URL
    ///     let url = "ws://example.com/socket".parse()?;
    ///
    ///     // Perform the WebSocket handshake over the existing stream
    ///     let ws = WebSocket::handshake(url, stream, Options::default()).await?;
    ///
    ///     // Now you can use the WebSocket connection
    ///     // ws.send(...).await?;
    ///
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Use Cases
    ///
    /// Use this function when you need to:
    /// - Use a custom connection method (e.g., SOCKS proxy, custom DNS resolution)
    /// - Reuse an existing stream or connection
    /// - Implement custom connection logic before the WebSocket handshake
    ///
    /// For adding custom headers to the handshake request, use
    /// [`WebSocket::handshake_with_request`] instead.
    pub async fn handshake(url: Url, io: S, options: Options) -> Result<WebSocket<S>> {
        Self::handshake_with_request(url, io, options, HttpRequest::builder()).await
    }

    /// Performs a WebSocket handshake with a customizable HTTP request.
    ///
    /// This is similar to [`WebSocket::handshake`] but allows you to customize
    /// the HTTP upgrade request by providing your own [`HttpRequestBuilder`].
    /// This is useful when you need to add custom headers (e.g., authentication
    /// tokens, API keys, or other metadata) to the handshake request.
    ///
    /// # Arguments
    ///
    /// * `url` - The WebSocket URL (used for generating handshake headers)
    /// * `io` - An existing I/O stream that implements AsyncRead + AsyncWrite
    /// * `options` - WebSocket configuration options
    /// * `builder` - An HTTP request builder for customizing the handshake request
    ///
    /// # Example
    ///
    /// ```no_run
    /// use tokio::net::TcpStream;
    /// use yawc::{WebSocket, Options, HttpRequest};
    ///
    /// #[tokio::main]
    /// async fn main() -> yawc::Result<()> {
    ///     // Establish your own TCP connection
    ///     let stream = TcpStream::connect("example.com:80").await?;
    ///
    ///     // Parse the WebSocket URL
    ///     let url = "ws://example.com/socket".parse()?;
    ///
    ///     // Create a custom HTTP request with authentication headers
    ///     let request = HttpRequest::builder()
    ///         .header("Authorization", "Bearer my-secret-token")
    ///         .header("X-Custom-Header", "custom-value");
    ///
    ///     // Perform the WebSocket handshake with custom headers
    ///     let ws = WebSocket::handshake_with_request(
    ///         url,
    ///         stream,
    ///         Options::default(),
    ///         request
    ///     ).await?;
    ///
    ///     // Now you can use the WebSocket connection
    ///     // ws.send(...).await?;
    ///
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Use Cases
    ///
    /// Use this function when you need to:
    /// - Add authentication headers to the handshake request
    /// - Include custom metadata or API keys
    /// - Control the exact HTTP request sent during the WebSocket upgrade
    /// - Combine custom connection logic with custom headers
    pub async fn handshake_with_request(
        url: Url,
        io: S,
        options: Options,
        builder: HttpRequestBuilder,
    ) -> Result<WebSocket<S>> {
        let (upgraded, negotiated) = http1_upgrade(url, io, options, builder).await?;

        // Recover the caller's own stream, along with anything the codec read past the
        // end of the response.
        let parts = upgraded
            .downcast::<TokioIo<S>>()
            .expect("downcast own stream");
        let stream = parts.io.into_inner();

        Ok(WebSocket::new(
            Role::Client,
            stream,
            parts.read_buf,
            negotiated,
        ))
    }
}

/// Runs the RFC 6455 handshake and returns hyper's upgraded stream.
///
/// The two client entry points differ only in what they do with that stream:
/// [`WebSocket::handshake_with_request`] downcasts it back to the caller's own type,
/// while the version-selecting path keeps it as an [`HttpStream`] so both HTTP versions
/// produce one type. Everything before that, defaulting the `Host` header, building the
/// request, offering compression, driving the connection and checking the response, is
/// the same work and lives here.
async fn http1_upgrade<S>(
    url: Url,
    io: S,
    options: Options,
    mut builder: HttpRequestBuilder,
) -> Result<(Upgraded, Negotiation)>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    if !builder
        .headers_ref()
        .expect("header")
        .contains_key(header::HOST)
    {
        let host = url.host().expect("hostname").to_string();

        let is_port_defined = url.port().is_some();
        let port = url.port_or_known_default().expect("port");
        let host_header = if is_port_defined {
            format!("{host}:{port}")
        } else {
            host
        };

        builder = builder.header(header::HOST, host_header.as_str());
    }

    let target_url = &url[url::Position::BeforePath..];

    let mut req = builder
        .method("GET")
        .uri(target_url)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "upgrade")
        .header(header::SEC_WEBSOCKET_KEY, generate_key())
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .body(Empty::<Bytes>::new())
        .expect("request build");

    if let Some(compression) = options.compression.as_ref() {
        let extensions = WebSocketExtensions::from(compression);
        let header_value = extensions.to_string().parse().expect("extensions header");
        req.headers_mut()
            .insert(header::SEC_WEBSOCKET_EXTENSIONS, header_value);
    }

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;

    spawn_connection(async move {
        if let Err(err) = conn.with_upgrades().await {
            log::debug!("http1 connection closed: {err:?}");
        }
    });

    let mut response = sender.send_request(req).await?;
    let negotiated = verify(&response, options)?;

    let upgraded = hyper::upgrade::on(&mut response).await?;

    Ok((upgraded, negotiated))
}

/// Drives a hyper connection in the background on whichever runtime is enabled.
///
/// Every client handshake needs this, and picking `tokio::spawn` unconditionally would
/// panic under the `smol` feature, where no tokio runtime is running.
pub(crate) fn spawn_connection<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    #[cfg(not(feature = "smol"))]
    tokio::spawn(fut);

    #[cfg(feature = "smol")]
    smol::spawn(fut).detach();
}

impl WebSocket<HttpStream> {
    /// Performs a WebSocket handshake when using the `reqwest` HTTP client.
    #[cfg(feature = "reqwest")]
    #[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
    pub async fn reqwest(
        mut url: Url,
        client: reqwest::Client,
        options: Options,
    ) -> Result<WebSocket<HttpStream>> {
        let host = url.host().expect("hostname").to_string();

        let host_header = if let Some(port) = url.port() {
            format!("{host}:{port}")
        } else {
            host
        };

        match url.scheme() {
            "ws" => {
                let _ = url.set_scheme("http");
            }
            "wss" => {
                let _ = url.set_scheme("https");
            }
            _ => {}
        }

        let req = client
            .get(url.as_str())
            .header(reqwest::header::HOST, host_header.as_str())
            .header(reqwest::header::UPGRADE, "websocket")
            .header(reqwest::header::CONNECTION, "upgrade")
            .header(reqwest::header::SEC_WEBSOCKET_KEY, generate_key())
            .header(reqwest::header::SEC_WEBSOCKET_VERSION, "13");

        let req = if let Some(compression) = options.compression.as_ref() {
            let extensions = WebSocketExtensions::from(compression);
            req.header(
                reqwest::header::SEC_WEBSOCKET_EXTENSIONS,
                extensions.to_string(),
            )
        } else {
            req
        };

        let response = req.send().await?;
        let negotiated = verify_reqwest(&response, options)?;

        let upgraded = response.upgrade().await?;

        Ok(WebSocket::new(
            Role::Client,
            HttpStream::from(upgraded),
            Bytes::new(),
            negotiated,
        ))
    }

    // ================== Server ====================

    /// Upgrades an HTTP connection to a WebSocket one.
    pub fn upgrade<B>(request: impl BorrowMut<Request<B>>) -> UpgradeResult {
        Self::upgrade_with_options(request, Options::default())
    }

    /// Attempts to upgrade an incoming `hyper::Request` to a WebSocket connection with customizable options.
    ///
    /// Handles both the HTTP/1.1 `Upgrade` handshake and, with the `http2` feature, the
    /// RFC 8441 extended CONNECT handshake. The protocol is picked from the request, so
    /// the same handler serves both.
    ///
    /// Accepting extended CONNECT also requires calling `enable_connect_protocol()` on
    /// the hyper HTTP/2 server builder. Without it hyper never delivers the request.
    pub fn upgrade_with_options<B>(
        mut request: impl BorrowMut<Request<B>>,
        options: Options,
    ) -> UpgradeResult {
        let request = request.borrow_mut();

        // Only the opening line of the response depends on which handshake was used.
        let builder = Self::handshake_response(request)?;

        if request
            .headers()
            .get(header::SEC_WEBSOCKET_VERSION)
            .map(|v| v.as_bytes())
            != Some(b"13")
        {
            return Err(WebSocketError::InvalidSecWebsocketVersion);
        }

        let client_extensions = WebSocketExtensions::from_headers(request.headers());
        let extensions =
            WebSocketExtensions::agree(options.compression.as_ref(), client_extensions);

        let builder = match extensions.as_ref() {
            Some(offer) => builder.header(header::SEC_WEBSOCKET_EXTENSIONS, offer.to_string()),
            None => builder,
        };

        let response = builder
            .body(Empty::new())
            .expect("bug: failed to build response");

        let stream = UpgradeFut {
            inner: hyper::upgrade::on(request),
            negotiation: Some(Negotiation::new(extensions, &options, Role::Server)?),
        };

        Ok((response, stream))
    }

    /// Starts the handshake response for whichever handshake the request used.
    ///
    /// This is the whole difference between the two on the server: RFC 8441 answers `200`
    /// and has no key to echo, RFC 6455 answers `101` and must echo one. What follows,
    /// the version check and extension negotiation, is the same either way.
    fn handshake_response<B>(request: &Request<B>) -> Result<hyper::http::response::Builder> {
        #[cfg(feature = "http2")]
        if http2::is_extended_connect(request.method(), request.extensions()) {
            return Ok(Response::builder().status(StatusCode::OK));
        }

        let key = request
            .headers()
            .get(header::SEC_WEBSOCKET_KEY)
            .ok_or(WebSocketError::MissingSecWebSocketKey)?;

        Ok(Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .header(
                header::SEC_WEBSOCKET_ACCEPT,
                upgrade::sec_websocket_protocol(key.as_bytes()),
            ))
    }
}

// ======== Generic WebSocket implementation =============

impl<S> WebSocket<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Splits the [`WebSocket`] into its low-level components for advanced usage.
    ///
    /// # Safety
    /// This function is unsafe because it splits ownership of shared state.
    pub unsafe fn split_stream(self) -> (Framed<S, Codec>, ReadHalf, WriteHalf) {
        self.streaming.split_stream()
    }

    /// Converts this `WebSocket` into a [`Streaming`] connection for manual fragment control.
    ///
    /// Use this when you need direct control over WebSocket frame fragmentation without
    /// automatic reassembly or fragmentation. This is useful for:
    /// - Streaming large files incrementally without loading them in memory
    /// - Implementing custom fragmentation strategies
    /// - Processing fragments as they arrive for low-latency applications
    /// - Fine-grained control over compression boundaries
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use yawc::{WebSocket, Frame};
    /// use futures::SinkExt;
    ///
    /// # async fn example() -> yawc::Result<()> {
    /// let ws = WebSocket::connect("wss://example.com/ws".parse()?).await?;
    /// let mut streaming = ws.into_streaming();
    ///
    /// // Send fragments manually
    /// streaming.send(Frame::text("Part 1").with_fin(false)).await?;
    /// streaming.send(Frame::continuation("Part 2")).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// See [`Streaming`] documentation and
    /// [`examples/streaming.rs`](https://github.com/infinitefield/yawc/blob/master/examples/streaming.rs)
    /// for more details.
    pub fn into_streaming(self) -> Streaming<S> {
        self.streaming
    }

    /// Polls for the next frame in the WebSocket stream.
    pub fn poll_next_frame(&mut self, cx: &mut Context<'_>) -> Poll<Result<Frame>> {
        loop {
            let frame = ready!(self.streaming.poll_next_frame(cx))?;
            match self.on_frame(frame)? {
                Some(ok) => break Poll::Ready(Ok(ok)),
                None => continue,
            }
        }
    }

    /// Asynchronously retrieves the next frame from the WebSocket stream.
    pub async fn next_frame(&mut self) -> Result<Frame> {
        poll_fn(|cx| self.poll_next_frame(cx)).await
    }

    /// Wraps a stream that has already completed a WebSocket handshake.
    ///
    /// Use this when something else performed the handshake and handed back a byte
    /// stream carrying WebSocket frames: a transport yawc does not implement, a custom
    /// HTTP client, or a proxy that has already switched protocols.
    ///
    /// No handshake is performed and no headers are inspected. The caller is responsible
    /// for having negotiated a compatible connection. `role` decides whether outgoing
    /// frames are masked ([`Role::Client`]) or not ([`Role::Server`]); getting it wrong
    /// produces frames the peer will reject.
    ///
    /// If the handshake negotiated `permessage-deflate`, use
    /// [`WebSocket::from_stream_with_extensions`] instead so the compression context is
    /// set up with the agreed parameters.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use yawc::{Options, Role, WebSocket};
    ///
    /// # async fn example(io: tokio::net::TcpStream) -> yawc::Result<()> {
    /// // `io` already carries WebSocket frames.
    /// let ws = WebSocket::from_stream(io, Role::Client, Options::default())?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_stream(io: S, role: Role, options: Options) -> Result<Self> {
        Self::from_stream_with_extensions(io, role, None, options)
    }

    /// Wraps an already-handshaked stream, honoring the extensions the peer agreed to.
    ///
    /// `extensions` is the raw `Sec-WebSocket-Extensions` header value as agreed by both
    /// sides, or `None` if no extensions were negotiated. Passing the value the handshake
    /// settled on matters because permessage-deflate parameters such as
    /// `server_max_window_bits` must match what the peer is using.
    ///
    /// See [`WebSocket::from_stream`] for the general contract.
    ///
    /// # Errors
    ///
    /// Returns an error if `extensions` names permessage-deflate but `options` did not
    /// enable compression, since there is no way to decode what the peer will send.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use yawc::{Options, Role, WebSocket};
    ///
    /// # async fn example(io: tokio::net::TcpStream) -> yawc::Result<()> {
    /// let ws = WebSocket::from_stream_with_extensions(
    ///     io,
    ///     Role::Client,
    ///     Some("permessage-deflate; client_max_window_bits=15"),
    ///     Options::default().with_balanced_compression(),
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_stream_with_extensions(
        io: S,
        role: Role,
        extensions: Option<&str>,
        options: Options,
    ) -> Result<Self> {
        let extensions = extensions
            .map(WebSocketExtensions::from_str)
            .and_then(std::result::Result::ok);

        let negotiation = Negotiation::new(extensions, &options, role)?;
        Ok(Self::new(role, io, Bytes::new(), negotiation))
    }

    /// Creates a new WebSocket from an existing stream.
    ///
    /// The `read_buf` parameter should contain any bytes that were read from the stream
    /// during the HTTP upgrade but weren't consumed (leftover data after the HTTP response).
    pub(crate) fn new(role: Role, stream: S, read_buf: Bytes, opts: Negotiation) -> Self {
        Self {
            streaming: Streaming::new(role, stream, read_buf, &opts),
            check_utf8: opts.utf8,
            fragment_layer: FragmentLayer::new(
                opts.fragmentation.as_ref().and_then(|f| f.fragment_size),
                opts.max_read_buffer,
                opts.fragmentation.as_ref().and_then(|f| f.timeout),
            ),
        }
    }

    fn on_frame(&mut self, frame: Frame) -> Result<Option<Frame>> {
        let frame = match self.fragment_layer.assemble_incoming(frame)? {
            Some(frame) => frame,
            None => return Ok(None), // Still waiting for more fragments
        };

        if frame.opcode == OpCode::Text && self.check_utf8 {
            #[cfg(not(feature = "simd"))]
            if std::str::from_utf8(&frame.payload).is_err() {
                return Err(WebSocketError::InvalidUTF8);
            }
            #[cfg(feature = "simd")]
            if simdutf8::basic::from_utf8(&frame.payload).is_err() {
                return Err(WebSocketError::InvalidUTF8);
            }
        }

        Ok(Some(frame))
    }
}

impl<S> futures::Stream for WebSocket<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Item = Frame;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match ready!(this.poll_next_frame(cx)) {
            Ok(ok) => Poll::Ready(Some(ok)),
            Err(_) => Poll::Ready(None),
        }
    }
}

impl<S> futures::Sink<Frame> for WebSocket<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Error = WebSocketError;

    fn poll_ready(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        let this = self.get_mut();
        this.streaming.poll_ready_unpin(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Frame) -> std::result::Result<(), Self::Error> {
        let this = self.get_mut();
        this.fragment_layer.fragment_outgoing(item);
        Ok(())
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        let this = self.get_mut();

        // First, send all queued fragments to WriteHalf
        while this.fragment_layer.has_outgoing_fragments() {
            // We need to call `poll_ready` before calling `start_send` since the user
            // might be under certain backpressure constraints
            ready!(this.streaming.poll_ready_unpin(cx))?;
            let fragment = this
                .fragment_layer
                .pop_outgoing_fragment()
                .expect("fragment");
            this.streaming.start_send_unpin(fragment)?;
        }

        // Then flush WriteHalf
        this.streaming.poll_flush_unpin(cx)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        let this = self.get_mut();
        this.streaming.poll_close_unpin(cx)
    }
}

// ================ Helper functions ====================

#[cfg(feature = "reqwest")]
fn verify_reqwest(response: &reqwest::Response, options: Options) -> Result<Negotiation> {
    if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        return Err(WebSocketError::InvalidStatusCode(
            response.status().as_u16(),
        ));
    }

    let headers = response.headers();

    if !headers
        .get(reqwest::header::UPGRADE)
        .and_then(|h| h.to_str().ok())
        .map(|h| h.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
    {
        return Err(WebSocketError::InvalidUpgradeHeader);
    }

    if !headers
        .get(reqwest::header::CONNECTION)
        .and_then(|h| h.to_str().ok())
        .map(|h| h.eq_ignore_ascii_case("Upgrade"))
        .unwrap_or(false)
    {
        return Err(WebSocketError::InvalidConnectionHeader);
    }

    let extensions = WebSocketExtensions::from_headers(headers);

    Negotiation::new(extensions, &options, Role::Client)
}

fn verify(response: &Response<Incoming>, options: Options) -> Result<Negotiation> {
    // Detect HTTP redirects before checking for 101.
    if response.status().is_redirection() {
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        return Err(WebSocketError::Redirected {
            status_code: response.status().as_u16(),
            location,
        });
    }

    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(WebSocketError::InvalidStatusCode(
            response.status().as_u16(),
        ));
    }

    let headers = response.headers();

    if !headers
        .get(header::UPGRADE)
        .and_then(|h| h.to_str().ok())
        .map(|h| h.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
    {
        return Err(WebSocketError::InvalidUpgradeHeader);
    }

    if !headers
        .get(header::CONNECTION)
        .and_then(|h| h.to_str().ok())
        .map(|h| h.eq_ignore_ascii_case("Upgrade"))
        .unwrap_or(false)
    {
        return Err(WebSocketError::InvalidConnectionHeader);
    }

    let extensions = WebSocketExtensions::from_headers(headers);

    Negotiation::new(extensions, &options, Role::Client)
}

fn generate_key() -> String {
    use base64::prelude::*;
    let input: [u8; 16] = rand::random();
    BASE64_STANDARD.encode(input)
}

/// Connects and handshakes using the HTTP version selected on the builder.
///
/// Unlike [`WebSocket::connect_priv`] this always resolves to an [`HttpWebSocket`],
/// because an HTTP/2 WebSocket is one stream of a multiplexed connection and there is no
/// socket to hand back. Both versions therefore produce the same type.
#[cfg(feature = "http2")]
async fn connect_versioned(
    url: Url,
    tcp_address: Option<SocketAddr>,
    connector: Option<TlsConnector>,
    options: Options,
    builder: HttpRequestBuilder,
    version: HttpVersion,
) -> Result<HttpWebSocket> {
    let host = url.host().expect("hostname").to_string();

    let tcp_stream = if let Some(tcp_address) = tcp_address {
        TcpStream::connect(tcp_address).await?
    } else {
        let port = url.port_or_known_default().expect("port");
        TcpStream::connect(format!("{host}:{port}")).await?
    };

    let _ = tcp_stream.set_nodelay(options.no_delay);

    // Over plaintext there is no ALPN to negotiate with, so HTTP/2 means prior knowledge.
    let stream = match url.scheme() {
        "ws" => MaybeTlsStream::Plain(tcp_stream),
        "wss" => {
            let connector = connector.unwrap_or_else(|| tls_connector_with_alpn(alpn_for(version)));
            let domain = ServerName::try_from(host)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid dnsname"))?;

            MaybeTlsStream::Tls(connector.connect(domain, tcp_stream).await?)
        }
        _ => return Err(WebSocketError::InvalidHttpScheme),
    };

    // Whatever the caller asked for is what runs. There is nothing to negotiate: a peer
    // that speaks h2 usually still wants WebSockets over HTTP/1.1, so picking HTTP/2 on
    // the strength of ALPN alone would be wrong more often than right. That is why the
    // HTTP/2 handshake is opt-in and why it fails here rather than quietly downgrading.
    match version {
        HttpVersion::Http2 => http2::handshake(url, stream, options, builder).await,
        HttpVersion::Http1 => handshake_http1_upgraded(url, stream, options, builder).await,
    }
}

/// Runs the HTTP/1.1 handshake but keeps hyper's upgraded stream instead of downcasting.
///
/// [`WebSocket::handshake_with_request`] recovers the original stream type so callers get
/// their socket back. The version-selecting path cannot do that, since it has to produce
/// the same type whichever HTTP version the handshake settles on.
#[cfg(feature = "http2")]
async fn handshake_http1_upgraded<S>(
    url: Url,
    io: S,
    options: Options,
    builder: HttpRequestBuilder,
) -> Result<HttpWebSocket>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (upgraded, negotiated) = http1_upgrade(url, io, options, builder).await?;

    Ok(WebSocket::new(
        Role::Client,
        HttpStream::from(TokioIo::new(upgraded)),
        Bytes::new(),
        negotiated,
    ))
}

/// Returns the ALPN protocols to offer for a given HTTP version preference.
#[cfg(feature = "http2")]
fn alpn_for(version: HttpVersion) -> Vec<Vec<u8>> {
    match version {
        HttpVersion::Http1 => vec![b"http/1.1".to_vec()],
        HttpVersion::Http2 => vec![b"h2".to_vec()],
    }
}

/// Creates a TLS connector with root certificates for secure WebSocket connections.
fn tls_connector() -> TlsConnector {
    tls_connector_with_alpn(vec![b"http/1.1".to_vec()])
}

/// Creates a TLS connector offering the given ALPN protocols.
fn tls_connector_with_alpn(alpn_protocols: Vec<Vec<u8>>) -> TlsConnector {
    let mut root_cert_store = rustls::RootCertStore::empty();
    root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| TrustAnchor {
        subject: ta.subject.clone(),
        subject_public_key_info: ta.subject_public_key_info.clone(),
        name_constraints: ta.name_constraints.clone(),
    }));

    let maybe_provider = rustls::crypto::CryptoProvider::get_default().cloned();

    #[cfg(any(feature = "rustls-ring", feature = "rustls-aws-lc-rs"))]
    let provider = maybe_provider.unwrap_or_else(|| {
        // Use ring as default if both are enabled, user can override with custom connector
        #[cfg(all(feature = "rustls-ring", not(feature = "rustls-aws-lc-rs")))]
        return Arc::new(rustls::crypto::ring::default_provider());

        #[cfg(all(feature = "rustls-aws-lc-rs", not(feature = "rustls-ring")))]
        return Arc::new(rustls::crypto::aws_lc_rs::default_provider());

        #[cfg(all(feature = "rustls-ring", feature = "rustls-aws-lc-rs"))]
        return Arc::new(rustls::crypto::ring::default_provider());
    });

    #[cfg(not(any(feature = "rustls-ring", feature = "rustls-aws-lc-rs")))]
    let provider = maybe_provider.expect(
        r#"No Rustls crypto provider was enabled for yawc to connect to a `wss://` endpoint!

Either:
    - provide a `connector` in the WebSocketBuilder options
    - enable one of the following features: `rustls-ring`, `rustls-aws-lc-rs`"#,
    );

    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::ALL_VERSIONS)
        .expect("versions")
        .with_root_certificates(root_cert_store)
        .with_no_client_auth();
    config.alpn_protocols = alpn_protocols;

    TlsConnector::from(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use crate::close::{self, CloseCode};

    use super::*;
    use futures::SinkExt;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};

    /// A mock duplex stream that wraps tokio's DuplexStream for testing.
    struct MockStream {
        inner: DuplexStream,
    }

    impl MockStream {
        /// Creates a pair of connected mock streams.
        fn pair(buffer_size: usize) -> (Self, Self) {
            let (a, b) = tokio::io::duplex(buffer_size);
            (Self { inner: a }, Self { inner: b })
        }
    }

    impl AsyncRead for MockStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for MockStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// Helper function to create a WebSocket pair for testing.
    fn create_websocket_pair(buffer_size: usize) -> (WebSocket<MockStream>, WebSocket<MockStream>) {
        create_websocket_pair_with_config(buffer_size, None, None)
    }

    fn create_websocket_pair_with_config(
        buffer_size: usize,
        fragment_size: Option<usize>,
        compression_level: Option<CompressionLevel>,
    ) -> (WebSocket<MockStream>, WebSocket<MockStream>) {
        let (client_stream, server_stream) = MockStream::pair(buffer_size);

        let extensions = compression_level.map(|_level| WebSocketExtensions::default());

        let mut options = Options::default();
        if let Some(level) = compression_level {
            options = options.with_compression_level(level);
        }
        if let Some(size) = fragment_size {
            options = options.with_max_fragment_size(size);
        }

        let negotiation =
            |role| Negotiation::new(extensions.clone(), &options, role).expect("negotiation");

        let client_ws = WebSocket::new(
            Role::Client,
            client_stream,
            Bytes::new(),
            negotiation(Role::Client),
        );

        let server_ws = WebSocket::new(
            Role::Server,
            server_stream,
            Bytes::new(),
            negotiation(Role::Server),
        );

        (client_ws, server_ws)
    }

    #[tokio::test]
    async fn test_send_and_receive_text_frame() {
        let (mut client, mut server) = create_websocket_pair(1024);

        let text = "Hello, WebSocket!";
        client
            .send(Frame::text(text))
            .await
            .expect("Failed to send text frame");

        let frame = server.next_frame().await.expect("Failed to receive frame");

        assert_eq!(frame.opcode(), OpCode::Text);
        assert_eq!(frame.payload(), text.as_bytes());
        assert!(frame.is_fin());
    }

    #[tokio::test]
    async fn test_send_and_receive_binary_frame() {
        let (mut client, mut server) = create_websocket_pair(1024);

        let data = vec![1u8, 2, 3, 4, 5];
        client
            .send(Frame::binary(data.clone()))
            .await
            .expect("Failed to send binary frame");

        let frame = server.next_frame().await.expect("Failed to receive frame");

        assert_eq!(frame.opcode(), OpCode::Binary);
        assert_eq!(frame.payload(), &data[..]);
        assert!(frame.is_fin());
    }

    #[tokio::test]
    async fn test_bidirectional_communication() {
        let (mut client, mut server) = create_websocket_pair(2048);

        client
            .send(Frame::text("Client message"))
            .await
            .expect("Failed to send from client");

        let frame = server
            .next_frame()
            .await
            .expect("Failed to receive at server");
        assert_eq!(frame.payload(), b"Client message" as &[u8]);

        server
            .send(Frame::text("Server response"))
            .await
            .expect("Failed to send from server");

        let frame = client
            .next_frame()
            .await
            .expect("Failed to receive at client");
        assert_eq!(frame.payload(), b"Server response" as &[u8]);
    }

    #[tokio::test]
    async fn test_ping_pong() {
        let (mut client, mut server) = create_websocket_pair(1024);

        // Ping frames are handled automatically by the WebSocket implementation
        // The server will automatically respond with a pong, but we won't receive it via next_frame()
        // Instead, test that we can send and receive pong frames explicitly

        client
            .send(Frame::pong("pong_data"))
            .await
            .expect("Failed to send pong");

        let frame = server.next_frame().await.expect("Failed to receive pong");
        assert_eq!(frame.opcode(), OpCode::Pong);
        assert_eq!(frame.payload(), b"pong_data" as &[u8]);
    }

    #[tokio::test]
    async fn test_close_frame() {
        let (mut client, mut server) = create_websocket_pair(1024);

        client
            .send(Frame::close(CloseCode::Normal, b"Goodbye"))
            .await
            .expect("Failed to send close frame");

        let frame = server
            .next_frame()
            .await
            .expect("Failed to receive close frame");

        assert_eq!(frame.opcode(), OpCode::Close);
        assert_eq!(frame.close_code(), Some(close::CloseCode::Normal));
        assert_eq!(
            frame.close_reason().expect("Invalid close reason"),
            Some("Goodbye")
        );
    }

    #[tokio::test]
    async fn test_large_message() {
        let (mut client, mut server) = create_websocket_pair(65536);

        let large_data = vec![42u8; 10240];
        client
            .send(Frame::binary(large_data.clone()))
            .await
            .expect("Failed to send large message");

        let frame = server
            .next_frame()
            .await
            .expect("Failed to receive large message");

        assert_eq!(frame.opcode(), OpCode::Binary);
        assert_eq!(frame.payload().len(), 10240);
        assert_eq!(frame.payload(), &large_data[..]);
    }

    #[tokio::test]
    async fn test_multiple_messages() {
        let (mut client, mut server) = create_websocket_pair(4096);

        for i in 0..10 {
            let msg = format!("Message {}", i);
            client
                .send(Frame::text(msg.clone()))
                .await
                .expect("Failed to send message");

            let frame = server
                .next_frame()
                .await
                .expect("Failed to receive message");
            assert_eq!(frame.payload(), msg.as_bytes());
        }
    }

    #[tokio::test]
    async fn test_empty_payload() {
        let (mut client, mut server) = create_websocket_pair(1024);

        client
            .send(Frame::text(Bytes::new()))
            .await
            .expect("Failed to send empty frame");

        let frame = server
            .next_frame()
            .await
            .expect("Failed to receive empty frame");

        assert_eq!(frame.opcode(), OpCode::Text);
        assert_eq!(frame.payload().len(), 0);
    }

    #[tokio::test]
    async fn test_fragmented_message() {
        let (mut client, mut server) = create_websocket_pair(2048);

        let mut frame1 = Frame::text("Hello, ");
        frame1.set_fin(false);
        client
            .send(frame1)
            .await
            .expect("Failed to send first fragment");

        let frame2 = Frame::continuation("World!");
        client
            .send(frame2)
            .await
            .expect("Failed to send final fragment");

        // WebSocket automatically reassembles fragments
        // We receive one complete message with the concatenated payload
        let received = server
            .next_frame()
            .await
            .expect("Failed to receive message");
        assert_eq!(received.opcode(), OpCode::Text);
        assert!(received.is_fin());
        assert_eq!(received.payload(), b"Hello, World!" as &[u8]);
    }

    #[tokio::test]
    async fn test_concurrent_send_receive() {
        let (mut client, mut server) = create_websocket_pair(4096);

        let client_task = tokio::spawn(async move {
            for i in 0..5 {
                client
                    .send(Frame::text(format!("Client {}", i)))
                    .await
                    .expect("Failed to send from client");

                let frame = client
                    .next_frame()
                    .await
                    .expect("Failed to receive at client");
                assert_eq!(frame.payload(), format!("Server {}", i).as_bytes());
            }
            client
        });

        let server_task = tokio::spawn(async move {
            for i in 0..5 {
                let frame = server
                    .next_frame()
                    .await
                    .expect("Failed to receive at server");
                assert_eq!(frame.payload(), format!("Client {}", i).as_bytes());

                server
                    .send(Frame::text(format!("Server {}", i)))
                    .await
                    .expect("Failed to send from server");
            }
            server
        });

        client_task.await.expect("Client task failed");
        server_task.await.expect("Server task failed");
    }

    #[tokio::test]
    async fn test_utf8_validation() {
        let (mut client, mut server) = create_websocket_pair(1024);

        let valid_utf8 = "Hello, 世界! 🌍";
        client
            .send(Frame::text(valid_utf8))
            .await
            .expect("Failed to send UTF-8 text");

        let frame = server
            .next_frame()
            .await
            .expect("Failed to receive UTF-8 text");
        assert_eq!(frame.opcode(), OpCode::Text);
        assert!(frame.is_utf8());
        assert_eq!(std::str::from_utf8(frame.payload()).unwrap(), valid_utf8);
    }

    #[tokio::test]
    async fn test_stream_trait_implementation() {
        use futures::StreamExt;

        let (mut client, mut server) = create_websocket_pair(1024);

        tokio::spawn(async move {
            for i in 0..3 {
                client
                    .send(Frame::text(format!("Message {}", i)))
                    .await
                    .expect("Failed to send message");
            }
        });

        let mut count = 0;
        while let Some(frame) = server.next().await {
            assert_eq!(frame.opcode(), OpCode::Text);
            count += 1;
            if count == 3 {
                break;
            }
        }
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_sink_trait_implementation() {
        use futures::SinkExt;

        let (mut client, mut server) = create_websocket_pair(1024);

        client
            .send(Frame::text("Sink message"))
            .await
            .expect("Failed to send via Sink");

        client.flush().await.expect("Failed to flush");

        let frame = server
            .next_frame()
            .await
            .expect("Failed to receive message");
        assert_eq!(frame.payload(), b"Sink message" as &[u8]);
    }

    #[tokio::test]
    async fn test_rapid_small_messages() {
        let (mut client, mut server) = create_websocket_pair(8192);

        let count = 100;

        let sender = tokio::spawn(async move {
            for i in 0..count {
                client
                    .send(Frame::text(format!("{}", i)))
                    .await
                    .expect("Failed to send");
            }
            client
        });

        for i in 0..count {
            let frame = server.next_frame().await.expect("Failed to receive");
            assert_eq!(frame.payload(), format!("{}", i).as_bytes());
        }

        sender.await.expect("Sender task failed");
    }

    #[tokio::test]
    async fn test_interleaved_control_and_data_frames() {
        let (mut client, mut server) = create_websocket_pair(2048);

        client
            .send(Frame::text("Data 1"))
            .await
            .expect("Failed to send");

        // Ping frames are handled automatically and don't appear in next_frame()
        // Use pong frames instead to test control frame interleaving
        client
            .send(Frame::pong("pong"))
            .await
            .expect("Failed to send pong");

        client
            .send(Frame::binary(vec![1, 2, 3]))
            .await
            .expect("Failed to send");

        let f1 = server.next_frame().await.expect("Failed to receive");
        assert_eq!(f1.opcode(), OpCode::Text);
        assert_eq!(f1.payload(), b"Data 1" as &[u8]);

        let f2 = server.next_frame().await.expect("Failed to receive");
        assert_eq!(f2.opcode(), OpCode::Pong);

        let f3 = server.next_frame().await.expect("Failed to receive");
        assert_eq!(f3.opcode(), OpCode::Binary);
        assert_eq!(f3.payload(), &[1u8, 2, 3] as &[u8]);
    }

    #[tokio::test]
    async fn test_client_sends_masked_frames() {
        let (mut client, mut _server) = create_websocket_pair(1024);

        // Create a frame and send it through the client
        let frame = Frame::text("test");
        client.send(frame).await.expect("Failed to send");

        // The frame should be automatically masked by the client encoder
        // We can't directly verify this without inspecting the wire format,
        // but the test verifies the codec path works correctly
    }

    #[tokio::test]
    async fn test_server_sends_unmasked_frames() {
        let (mut _client, mut server) = create_websocket_pair(1024);

        // Server frames should not be masked
        let frame = Frame::text("test");
        server.send(frame).await.expect("Failed to send");

        // Similar to above - verifies the codec path
    }

    #[tokio::test]
    async fn test_close_code_variants() {
        let (mut client, mut server) = create_websocket_pair(1024);

        client
            .send(Frame::close(close::CloseCode::Away, b""))
            .await
            .expect("Failed to send close");

        let frame = server.next_frame().await.expect("Failed to receive");
        assert_eq!(frame.close_code(), Some(close::CloseCode::Away));
    }

    #[tokio::test]
    async fn test_multiple_fragments() {
        let (mut client, mut server) = create_websocket_pair(4096);

        // Send 5 fragments
        for i in 0..5 {
            let is_last = i == 4;
            let opcode = if i == 0 {
                OpCode::Text
            } else {
                OpCode::Continuation
            };

            let mut frame = Frame::from((opcode, format!("part{}", i)));
            frame.set_fin(is_last);
            client.send(frame).await.expect("Failed to send fragment");
        }

        // WebSocket automatically reassembles fragments
        // We receive one complete message, not individual fragments
        let frame = server.next_frame().await.expect("Failed to receive");
        assert_eq!(frame.opcode(), OpCode::Text);
        assert!(frame.is_fin());

        // The payload should be the concatenation of all fragments
        let expected = "part0part1part2part3part4";
        assert_eq!(frame.payload(), expected.as_bytes());
    }

    #[tokio::test]
    async fn test_automatic_fragmentation_large_messages() {
        // Create WebSocket pair with fragment_size set to 100 bytes
        let (client_stream, server_stream) = MockStream::pair(8192);

        let options = Options::default().with_max_fragment_size(100);
        let negotiation = |role| Negotiation::new(None, &options, role).expect("negotiation");

        let mut client_ws = WebSocket::new(
            Role::Client,
            client_stream,
            Bytes::new(),
            negotiation(Role::Client),
        );

        let mut server_ws = WebSocket::new(
            Role::Server,
            server_stream,
            Bytes::new(),
            negotiation(Role::Server),
        );

        // Send a large message (300 bytes) from client
        let large_payload = vec![b'A'; 300];
        client_ws
            .send(Frame::binary(large_payload.clone()))
            .await
            .unwrap();

        // Server should receive the complete message (reassembled from fragments)
        let received = server_ws.next_frame().await.unwrap();
        assert_eq!(received.opcode(), OpCode::Binary);
        assert_eq!(received.payload(), large_payload.as_slice());
    }

    #[tokio::test]
    async fn test_automatic_fragmentation_small_messages() {
        // Create WebSocket pair with fragment_size set to 100 bytes
        let (client_stream, server_stream) = MockStream::pair(8192);

        let options = Options::default().with_max_fragment_size(100);
        let negotiation = |role| Negotiation::new(None, &options, role).expect("negotiation");

        let mut client_ws = WebSocket::new(
            Role::Client,
            client_stream,
            Bytes::new(),
            negotiation(Role::Client),
        );

        let mut server_ws = WebSocket::new(
            Role::Server,
            server_stream,
            Bytes::new(),
            negotiation(Role::Server),
        );

        // Send a small message (50 bytes) from client
        let small_payload = vec![b'B'; 50];
        client_ws
            .send(Frame::text(small_payload.clone()))
            .await
            .unwrap();

        // Server should receive the complete message
        let received = server_ws.next_frame().await.unwrap();
        assert_eq!(received.opcode(), OpCode::Text);
        assert_eq!(received.payload(), small_payload.as_slice());
    }

    #[tokio::test]
    async fn test_no_fragmentation_when_not_configured() {
        // Create WebSocket pair WITHOUT fragment_size
        let (client_stream, server_stream) = MockStream::pair(8192);

        let options = Options::default();
        let negotiation = |role| Negotiation::new(None, &options, role).expect("negotiation");

        let mut client_ws = WebSocket::new(
            Role::Client,
            client_stream,
            Bytes::new(),
            negotiation(Role::Client),
        );

        let mut server_ws = WebSocket::new(
            Role::Server,
            server_stream,
            Bytes::new(),
            negotiation(Role::Server),
        );

        // Send a large message (1000 bytes) without fragmentation config
        let large_payload = vec![b'C'; 1000];
        client_ws
            .send(Frame::binary(large_payload.clone()))
            .await
            .unwrap();

        // Server should receive the complete message
        let received = server_ws.next_frame().await.unwrap();
        assert_eq!(received.opcode(), OpCode::Binary);
        assert_eq!(received.payload(), large_payload.as_slice());
    }

    #[tokio::test]
    async fn test_interleave_control_frames_with_continuation_frames() {
        // Per RFC 6455 Section 5.5:
        // "Control frames themselves MUST NOT be fragmented."
        // "Control frames MAY be injected in the middle of a fragmented message."
        //
        // This test verifies that control frames (ping/pong) can be interleaved
        // with continuation frames during message fragmentation, and that the
        // fragmented message is still correctly reassembled.
        let (mut client, mut server) = create_websocket_pair(4096);

        // Send first fragment of a text message (FIN=0)
        let mut fragment1 = Frame::text("Hello, ");
        fragment1.set_fin(false);
        client
            .send(fragment1)
            .await
            .expect("Failed to send first fragment");

        // Interleave a ping frame in the middle of the fragmented message
        client
            .send(Frame::ping("ping during fragmentation"))
            .await
            .expect("Failed to send ping");

        // Send second continuation fragment (FIN=0)
        let mut fragment2 = Frame::continuation("World");
        fragment2.set_fin(false);
        client
            .send(fragment2)
            .await
            .expect("Failed to send second fragment");

        // Interleave a pong frame
        client
            .send(Frame::pong("pong during fragmentation"))
            .await
            .expect("Failed to send pong");

        // Send final continuation fragment (FIN=1)
        let fragment3 = Frame::continuation("!");
        client
            .send(fragment3)
            .await
            .expect("Failed to send final fragment");

        // Server should receive the ping frame first
        let ping_frame = server
            .next_frame()
            .await
            .expect("Failed to receive ping frame");
        assert_eq!(ping_frame.opcode(), OpCode::Ping);
        assert_eq!(ping_frame.payload(), b"ping during fragmentation" as &[u8]);

        // Server should receive the pong frame
        let pong_frame = server
            .next_frame()
            .await
            .expect("Failed to receive pong frame");
        assert_eq!(pong_frame.opcode(), OpCode::Pong);
        assert_eq!(pong_frame.payload(), b"pong during fragmentation" as &[u8]);

        // Server should receive the complete reassembled message
        let message_frame = server
            .next_frame()
            .await
            .expect("Failed to receive reassembled message");
        assert_eq!(message_frame.opcode(), OpCode::Text);
        assert!(message_frame.is_fin());
        assert_eq!(message_frame.payload(), b"Hello, World!" as &[u8]);
    }

    #[tokio::test]
    async fn test_large_compressed_fragmented_payload() {
        // Test large payload with manual compression and fragmentation
        // Fragment size: 65536 bytes
        // This tests that:
        // 1. User can manually fragment large payloads using set_fin()
        // 2. Compression works across manually created fragments
        // 3. Decompression and reassembly produce the original payload

        const FRAGMENT_SIZE: usize = 65536;
        const PAYLOAD_SIZE: usize = 1024 * 1024; // 1 MB

        use flate2::Compression;

        let (mut client, mut server) = create_websocket_pair_with_config(
            256 * 1024, // 256 KB buffer
            None,       // No automatic fragmentation - we do it manually
            Some(Compression::best()),
        );

        // Create a large payload with repetitive data (compresses well)
        let payload: Vec<u8> = (0..PAYLOAD_SIZE).map(|i| (i % 256) as u8).collect();

        // Manually fragment the payload and send as separate frames
        let total_fragments = PAYLOAD_SIZE.div_ceil(FRAGMENT_SIZE);
        println!(
            "Sending {} bytes in {} fragments of {} bytes each",
            PAYLOAD_SIZE, total_fragments, FRAGMENT_SIZE
        );

        // Spawn server task to receive the message concurrently
        let server_task = tokio::spawn(async move {
            server
                .next_frame()
                .await
                .expect("Failed to receive large payload")
        });

        // Send all fragments from client
        let mut offset = 0;
        let mut fragment_num = 0;

        while offset < PAYLOAD_SIZE {
            let end = std::cmp::min(offset + FRAGMENT_SIZE, PAYLOAD_SIZE);
            let chunk = payload[offset..end].to_vec();
            let is_final = end == PAYLOAD_SIZE;

            let mut frame = if fragment_num == 0 {
                // First frame: use Binary opcode
                Frame::binary(chunk)
            } else {
                // Continuation frames
                Frame::continuation(chunk)
            };

            // Set FIN bit only on the last fragment
            frame.set_fin(is_final);

            println!(
                "Sending fragment {}/{}: {} bytes, OpCode={:?} FIN={}",
                fragment_num + 1,
                total_fragments,
                frame.payload().len(),
                frame.opcode(),
                is_final
            );

            client
                .send(frame)
                .await
                .unwrap_or_else(|_| panic!("Failed to send fragment {}", fragment_num + 1));

            offset = end;
            fragment_num += 1;
        }

        // Wait for server to receive the complete message
        let received_frame = server_task.await.expect("Server task failed");

        // Verify the payload was reassembled correctly
        assert_eq!(received_frame.opcode(), OpCode::Binary);
        assert!(received_frame.is_fin());
        assert_eq!(received_frame.payload().len(), PAYLOAD_SIZE);
        assert_eq!(received_frame.payload().as_ref(), &payload[..]);

        println!(
            "Successfully sent {} manual fragments, compressed, decompressed, and reassembled {} bytes",
            total_fragments, PAYLOAD_SIZE
        );
    }

    #[tokio::test]
    async fn test_compressed_fragmented_with_interleaved_control() {
        // Test compression + fragmentation + interleaved control frames
        // This is the most complex scenario combining:
        // 1. Compression (best quality)
        // 2. Automatic fragmentation
        // 3. Control frames between fragments

        const FRAGMENT_SIZE: usize = 65536;

        use flate2::Compression;

        let (mut client, mut server) = create_websocket_pair_with_config(
            128 * 1024,
            Some(FRAGMENT_SIZE),
            Some(Compression::best()),
        );

        // Create a payload that will span multiple fragments
        let payload = "This is a test payload that should compress well. ".repeat(5000);
        let original_payload = payload.clone();
        let payload_bytes = payload.as_bytes().to_vec();

        // Send the payload (will be fragmented automatically)
        tokio::spawn(async move {
            client
                .send(Frame::binary(payload_bytes))
                .await
                .expect("Failed to send payload");

            // Send a ping after starting the fragmented message
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            client
                .send(Frame::ping("test"))
                .await
                .expect("Failed to send ping");
        });

        // Server should be able to receive the fragmented message and control frames
        let mut received_message = None;
        let mut received_ping = false;

        for _ in 0..2 {
            let frame = server.next_frame().await.expect("Failed to receive frame");

            match frame.opcode() {
                OpCode::Binary => {
                    assert!(frame.is_fin());
                    received_message = Some(frame.payload().to_vec());
                }
                OpCode::Ping => {
                    received_ping = true;
                }
                _ => panic!("Unexpected frame type: {:?}", frame.opcode()),
            }
        }

        assert!(received_message.is_some(), "Message not received");
        assert!(received_ping, "Ping not received");

        let received = String::from_utf8(received_message.unwrap())
            .expect("Invalid UTF-8 in received payload");

        assert_eq!(
            received, original_payload,
            "Compressed fragmented payload mismatch"
        );
    }

    /// A client that terminates its deflate stream with a final block (BFINAL=1) used to
    /// spin the server's read task at 100% CPU instead of yielding the message.
    ///
    /// The read runs on a detached thread with its own runtime: the spin never yields, so
    /// it can neither be cancelled from within its own task nor joined at runtime
    /// shutdown. Going through a channel lets a regression fail on the timeout instead of
    /// hanging the whole test run.
    ///
    /// See https://github.com/infinitefield/yawc/issues/40
    #[test]
    fn test_final_block_compressed_frame_does_not_spin() {
        use std::sync::mpsc;
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;

        const TEXT: &str = r#"[2,"35364f26","TransactionEvent",{"eventType":"Started"}]"#;

        for client_no_context_takeover in [false, true] {
            let extensions = WebSocketExtensions {
                client_no_context_takeover,
                ..Default::default()
            };
            let options = Options::default()
                .with_compression_level(CompressionLevel::default())
                .with_utf8();
            let negotiation =
                Negotiation::new(Some(extensions), &options, Role::Server).expect("negotiation");

            let (mut raw_client, server_stream) = MockStream::pair(4096);
            let mut server = WebSocket::new(Role::Server, server_stream, Bytes::new(), negotiation);

            // Compress the way the reporter's client does: finish the deflate stream
            // (BFINAL=1) and leave trailing bytes the inflater can never consume.
            let mut encoder = flate2::Compress::new(flate2::Compression::default(), false);
            let mut payload = vec![0u8; 512];
            encoder
                .compress(TEXT.as_bytes(), &mut payload, flate2::FlushCompress::Finish)
                .expect("compression failed");
            payload.truncate(encoder.total_out() as usize);
            payload.extend_from_slice(&[0x0b, 0x5b, 0xb9, 0x68]);
            assert!(payload.len() < 126, "frame length must fit the 7-bit form");

            // FIN + RSV1 + Text, masked as a client frame.
            let mask = [0xa1, 0x15, 0x93, 0xf5];
            let mut bytes = vec![0xc1, 0x80 | payload.len() as u8];
            bytes.extend_from_slice(&mask);
            bytes.extend(
                payload
                    .iter()
                    .enumerate()
                    .map(|(i, byte)| byte ^ mask[i % 4]),
            );

            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");

                let result = runtime.block_on(async move {
                    raw_client.write_all(&bytes).await.expect("write frame");
                    raw_client.flush().await.expect("flush frame");

                    let frame = server.next_frame().await?;
                    Ok::<_, WebSocketError>((frame.opcode(), frame.payload().to_vec()))
                });

                let _ = tx.send(result);
            });

            let (opcode, payload) = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("server read spun instead of returning (issue #40)")
                .expect("failed to receive frame");

            assert_eq!(opcode, OpCode::Text);
            assert_eq!(payload, TEXT.as_bytes());
        }
    }

    /// A server must not be able to switch compression on for a client that never
    /// offered it. This reached an unwrap and panicked the client task before the
    /// negotiation was resolved in one place.
    ///
    /// RFC 6455, Section 4.1: a client must fail the connection when the response
    /// indicates an extension that was not present in its handshake.
    #[tokio::test]
    async fn test_unoffered_compression_fails_handshake() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const HANDSHAKE: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\n\
            upgrade: websocket\r\n\
            connection: upgrade\r\n\
            sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
            sec-websocket-extensions: permessage-deflate\r\n\
            \r\n";

        let (client_io, mut peer) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                match peer.read(&mut byte).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => request.push(byte[0]),
                }
            }
            let _ = peer.write_all(HANDSHAKE).await;
            // Hold the connection open so the failure comes from the negotiation.
            std::future::pending::<()>().await;
        });

        // Compression is not enabled on this side.
        let options = Options::default();
        assert!(options.compression.is_none());

        let result =
            WebSocket::handshake("ws://localhost/".parse().expect("url"), client_io, options).await;

        assert!(
            matches!(result, Err(WebSocketError::CompressionNotSupported)),
            "expected the handshake to fail, got a different outcome"
        );
    }
}
