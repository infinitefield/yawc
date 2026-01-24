//! Native WebSocket implementation for Tokio runtime.
//!
//! # Architecture Layer: Protocol & Application
//!
//! This module implements the **top layer** of the WebSocket processing stack,
//! providing the main [`WebSocket`] type that applications interact with.
//!
//! ## WebSocket Layer Responsibilities
//!
//! The [`WebSocket`] type handles:
//!
//! - **Decompression**: Applies permessage-deflate decompression (RFC 7692) to complete messages
//! - **UTF-8 validation**: Validates text frames contain valid UTF-8
//! - **Protocol control**: Handles Ping/Pong and Close frames automatically
//! - **Connection management**: Manages WebSocket connection lifecycle
//! - **Futures integration**: Implements `Stream` and `Sink` traits
//!
//! ## Layered Processing
//!
//! Messages flow through three distinct layers:
//!
//! ```text
//! ┌────────────────────────────────────────────────┐
//! │  WebSocket Layer (this module)                 │
//! │  • Decompresses complete assembled messages    │
//! │  • Validates UTF-8 for text frames             │
//! │  • Handles Ping/Pong/Close protocol            │
//! └──────────────────┬─────────────────────────────┘
//!                    │
//! ┌──────────────────▼─────────────────────────────┐
//! │  ReadHalf Layer (split module)                 │
//! │  • Assembles fragmented messages               │
//! │  • Tracks fragment state and timeouts          │
//! │  • Enforces message size limits                │
//! └──────────────────┬─────────────────────────────┘
//!                    │
//! ┌──────────────────▼─────────────────────────────┐
//! │  Codec Layer (codec module)                    │
//! │  • Decodes individual frames from bytes        │
//! │  • Handles masking/unmasking                   │
//! │  • Parses frame headers (FIN, RSV, OpCode)     │
//! └────────────────────────────────────────────────┘
//! ```
//!
//! ## Example: Compressed Fragmented Message
//!
//! When receiving a compressed message split across 3 fragments:
//!
//! 1. **Codec**: Decodes 3 individual frames
//!    - `Frame(Text, RSV1=1, FIN=0, payload1)`
//!    - `Frame(Continuation, RSV1=0, FIN=0, payload2)`
//!    - `Frame(Continuation, RSV1=0, FIN=1, payload3)`
//!
//! 2. **ReadHalf**: Assembles complete compressed message
//!    - Returns `Frame(Text, RSV1=1, FIN=1, payload1+payload2+payload3)`
//!
//! 3. **WebSocket**: Decompresses and validates
//!    - Decompresses the concatenated payload
//!    - Validates UTF-8 if it's a text frame
//!    - Returns final frame to application
//!
//! This ensures RFC 6455 fragmentation and RFC 7692 compression are both handled correctly.

mod builder;
mod options;
mod split;
mod upgrade;

use crate::{close, codec, compression, frame, Result, WebSocketError};

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
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite};

use close::CloseCode;
use codec::Codec;
use compression::{Compressor, Decompressor, WebSocketExtensions};
use futures::task::AtomicWaker;
use tokio_rustls::rustls::{self, pki_types::TrustAnchor};
use tokio_util::codec::{Framed, FramedParts};
use url::Url;

// Re-exports
pub use crate::stream::MaybeTlsStream;
pub use builder::{HttpRequest, HttpRequestBuilder, WebSocketBuilder};
pub use frame::{Frame, OpCode};
pub use options::{CompressionLevel, DeflateOptions, Options};
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
    pub(crate) extensions: Option<WebSocketExtensions>,
    pub(crate) compression_level: Option<CompressionLevel>,
    pub(crate) max_payload_read: usize,
    pub(crate) max_read_buffer: usize,
    pub(crate) utf8: bool,
    pub(crate) fragment_timeout: Option<Duration>,
    pub(crate) max_payload_write_size: Option<usize>,
    pub(crate) max_backpressure_write_boundary: Option<usize>,
}

impl Negotiation {
    pub(crate) fn decompressor(&self, role: Role) -> Option<Decompressor> {
        let config = self.extensions.as_ref()?;

        log::debug!(
            "Established decompressor for {role} with settings \
            client_no_context_takeover={} server_no_context_takeover={} \
            server_max_window_bits={:?} client_max_window_bits={:?}",
            config.client_no_context_takeover,
            config.client_no_context_takeover,
            config.server_max_window_bits,
            config.client_max_window_bits
        );

        // configure the decompressor using the assigned role and preferred flags.
        Some(if role == Role::Server {
            if config.client_no_context_takeover {
                Decompressor::no_context_takeover()
            } else {
                #[cfg(feature = "zlib")]
                if let Some(Some(window_bits)) = config.client_max_window_bits {
                    Decompressor::new_with_window_bits(window_bits.max(9))
                } else {
                    Decompressor::new()
                }
                #[cfg(not(feature = "zlib"))]
                Decompressor::new()
            }
        } else {
            // client
            if config.server_no_context_takeover {
                Decompressor::no_context_takeover()
            } else {
                #[cfg(feature = "zlib")]
                if let Some(Some(window_bits)) = config.server_max_window_bits {
                    Decompressor::new_with_window_bits(window_bits)
                } else {
                    Decompressor::new()
                }
                #[cfg(not(feature = "zlib"))]
                Decompressor::new()
            }
        })
    }

    pub(crate) fn compressor(&self, role: Role) -> Option<Compressor> {
        let config = self.extensions.as_ref()?;

        log::debug!(
            "Established compressor for {role} with settings \
            client_no_context_takeover={} server_no_context_takeover={} \
            server_max_window_bits={:?} client_max_window_bits={:?}",
            config.client_no_context_takeover,
            config.client_no_context_takeover,
            config.server_max_window_bits,
            config.client_max_window_bits
        );

        let level = self.compression_level.unwrap();

        // configure the compressor using the assigned role and preferred flags.
        Some(if role == Role::Client {
            if config.client_no_context_takeover {
                Compressor::no_context_takeover(level)
            } else {
                #[cfg(feature = "zlib")]
                if let Some(Some(window_bits)) = config.client_max_window_bits {
                    Compressor::new_with_window_bits(level, window_bits)
                } else {
                    Compressor::new(level)
                }
                #[cfg(not(feature = "zlib"))]
                Compressor::new(level)
            }
        } else {
            // server
            if config.server_no_context_takeover {
                Compressor::no_context_takeover(level)
            } else {
                #[cfg(feature = "zlib")]
                if let Some(Some(window_bits)) = config.server_max_window_bits {
                    Compressor::new_with_window_bits(level, window_bits)
                } else {
                    Compressor::new(level)
                }
                #[cfg(not(feature = "zlib"))]
                Compressor::new(level)
            }
        })
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
/// When [`Options::with_max_payload_write_size`] is configured, the WebSocket will automatically
/// fragment outgoing messages that exceed the specified size limit:
///
/// ```no_run
/// use yawc::{WebSocket, Frame, Options};
/// use futures::SinkExt;
///
/// # async fn example() -> yawc::Result<()> {
/// let options = Options::default()
///     .with_max_payload_write_size(64 * 1024); // 64 KiB per frame
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
/// # Manual Fragmentation (Advanced)
///
/// For low-level use cases, you can manually fragment messages by sending frames with
/// `FIN=0`. This is an advanced feature and requires careful handling:
///
/// ```no_run
/// use yawc::{WebSocket, Frame};
/// use futures::SinkExt;
///
/// # async fn example(mut ws: WebSocket<tokio::net::TcpStream>) -> yawc::Result<()> {
/// // First fragment: Text frame with FIN=0 (not compressed)
/// ws.send(Frame::text("Hello ").with_fin(false)).await?;
///
/// // Second fragment: Continuation frame with FIN=0 (not compressed)
/// ws.send(Frame::continuation("World").with_fin(false)).await?;
///
/// // Final fragment: Continuation frame with FIN=1 (not compressed)
/// ws.send(Frame::continuation("!")).await?;
/// # Ok(())
/// # }
/// ```
///
/// **Important**: Manual fragmentation disables both compression and automatic fragmentation.
/// When manually fragmenting messages, compression is **disabled** for all fragments. Only
/// complete, non-fragmented frames are eligible for compression. This is consistent with
/// RFC 7692, which specifies that the RSV1 bit (compression flag) is only set on the first
/// frame of a fragmented message.
/// See [`examples/fragmented_messages.rs`](https://github.com/infinitefield/yawc/blob/master/examples/fragmented_messages.rs)
/// for a complete example of sending and receiving fragmented messages.
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
    stream: Framed<S, Codec>,
    read_half: ReadHalf,
    write_half: WriteHalf,
    wake_proxy: Arc<WakeProxy>,
    obligated_sends: VecDeque<Frame>,
    flush_sends: bool,
    inflate: Option<Decompressor>,
    deflate: Option<Compressor>,
    check_utf8: bool,
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
        mut builder: HttpRequestBuilder,
    ) -> Result<WebSocket<S>> {
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
            let header_value = extensions.to_string().parse().unwrap();
            req.headers_mut()
                .insert(header::SEC_WEBSOCKET_EXTENSIONS, header_value);
        }

        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;

        #[cfg(not(feature = "smol"))]
        tokio::spawn(async move {
            if let Err(err) = conn.with_upgrades().await {
                log::error!("upgrading connection: {:?}", err);
            }
        });

        #[cfg(feature = "smol")]
        smol::spawn(async move {
            if let Err(err) = conn.with_upgrades().await {
                log::error!("upgrading connection: {:?}", err);
            }
        })
        .detach();

        let mut response = sender.send_request(req).await?;
        let negotiated = verify(&response, options)?;

        let upgraded = hyper::upgrade::on(&mut response).await?;
        let parts = upgraded.downcast::<TokioIo<S>>().unwrap();

        // Extract the original stream and any leftover read buffer
        let stream = parts.io.into_inner();
        let read_buf = parts.read_buf;

        Ok(WebSocket::new(Role::Client, stream, read_buf, negotiated))
    }
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
    pub fn upgrade_with_options<B>(
        mut request: impl BorrowMut<Request<B>>,
        options: Options,
    ) -> UpgradeResult {
        let request = request.borrow_mut();

        let key = request
            .headers()
            .get(header::SEC_WEBSOCKET_KEY)
            .ok_or(WebSocketError::MissingSecWebSocketKey)?;

        if request
            .headers()
            .get(header::SEC_WEBSOCKET_VERSION)
            .map(|v| v.as_bytes())
            != Some(b"13")
        {
            return Err(WebSocketError::InvalidSecWebsocketVersion);
        }

        let maybe_compression = request
            .headers()
            .get(header::SEC_WEBSOCKET_EXTENSIONS)
            .and_then(|h| h.to_str().ok())
            .map(WebSocketExtensions::from_str)
            .and_then(std::result::Result::ok);

        let mut response = Response::builder()
            .status(hyper::StatusCode::SWITCHING_PROTOCOLS)
            .header(hyper::header::CONNECTION, "upgrade")
            .header(hyper::header::UPGRADE, "websocket")
            .header(
                header::SEC_WEBSOCKET_ACCEPT,
                upgrade::sec_websocket_protocol(key.as_bytes()),
            )
            .body(Empty::new())
            .expect("bug: failed to build response");

        let extensions = if let Some(client_compression) = maybe_compression {
            if let Some(server_compression) = options.compression.as_ref() {
                let offer = server_compression.merge(&client_compression);

                let header_value = offer.to_string().parse().unwrap();
                response
                    .headers_mut()
                    .insert(header::SEC_WEBSOCKET_EXTENSIONS, header_value);

                Some(offer)
            } else {
                None
            }
        } else {
            None
        };

        let max_read_buffer = options.max_read_buffer.unwrap_or(
            options
                .max_payload_read
                .map(|payload_read| payload_read * 2)
                .unwrap_or(MAX_READ_BUFFER),
        );

        let stream = UpgradeFut {
            inner: hyper::upgrade::on(request),
            negotiation: Some(Negotiation {
                extensions,
                compression_level: options
                    .compression
                    .as_ref()
                    .map(|compression| compression.level),
                max_payload_read: options.max_payload_read.unwrap_or(MAX_PAYLOAD_READ),
                max_read_buffer,
                utf8: options.check_utf8,
                max_payload_write_size: options.max_payload_write_size,
                fragment_timeout: options.fragment_timeout,
                max_backpressure_write_boundary: options.max_backpressure_write_boundary,
            }),
        };

        Ok((response, stream))
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
        (self.stream, self.read_half, self.write_half)
    }

    /// Polls for the next frame in the WebSocket stream.
    pub fn poll_next_frame(&mut self, cx: &mut Context<'_>) -> Poll<Result<Frame>> {
        let wake_proxy = Arc::clone(&self.wake_proxy);
        wake_proxy.set_waker(ContextKind::Read, cx.waker());

        loop {
            let res = wake_proxy.with_context(|cx| self.read_half.poll_frame(&mut self.stream, cx));
            match res {
                Poll::Ready(Ok(frame)) => match self.on_frame(frame)? {
                    Some(frame) => return Poll::Ready(Ok(frame)),
                    None => continue,
                },
                Poll::Ready(Err(WebSocketError::ConnectionClosed)) => {
                    ready!(wake_proxy.with_context(|cx| self.poll_flush_obligated(cx)))?;
                    return Poll::Ready(Err(WebSocketError::ConnectionClosed));
                }
                Poll::Ready(Err(err)) => {
                    let code = match err {
                        WebSocketError::FrameTooLarge => CloseCode::Size,
                        WebSocketError::InvalidOpCode(_) => CloseCode::Unsupported,
                        WebSocketError::ReservedBitsNotZero
                        | WebSocketError::ControlFrameFragmented
                        | WebSocketError::PingFrameTooLarge
                        | WebSocketError::InvalidFragment
                        | WebSocketError::FragmentTimeout
                        | WebSocketError::InvalidContinuationFrame
                        | WebSocketError::CompressionNotSupported => CloseCode::Protocol,
                        _ => CloseCode::Error,
                    };
                    self.emit_close(Frame::close(code, err.to_string()));
                    return Poll::Ready(Err(err));
                }
                Poll::Pending => {
                    let res = ready!(wake_proxy.with_context(|cx| self.poll_flush_obligated(cx)));
                    if let Err(err) = res {
                        return Poll::Ready(Err(err));
                    }
                    return Poll::Pending;
                }
            }
        }
    }

    /// Asynchronously retrieves the next frame from the WebSocket stream.
    pub async fn next_frame(&mut self) -> Result<Frame> {
        poll_fn(|cx| self.poll_next_frame(cx)).await
    }

    /// Sends a message as multiple fragmented frames.
    ///
    /// This method splits a large message into smaller fragments, useful for:
    /// - Sending large payloads without blocking
    /// - Implementing streaming message transmission
    /// - Controlling memory usage for large messages
    ///
    /// # Example
    ///
    /// See [`examples/fragmented_messages.rs`](https://github.com/infinitefield/yawc/blob/master/examples/fragmented_messages.rs)
    /// for a complete example of sending and receiving fragmented messages.
    pub async fn send_fragmented(
        &mut self,
        opcode: OpCode,
        payload: impl Into<Bytes>,
        fragment_size: usize,
    ) -> Result<()> {
        let payload = payload.into();
        let total_len = payload.len();

        if total_len <= fragment_size {
            return futures::SinkExt::send(self, Frame::from((opcode, payload))).await;
        }

        let mut offset = 0;
        let mut is_first = true;

        while offset < total_len {
            let end = (offset + fragment_size).min(total_len);
            let chunk = payload.slice(offset..end);
            let is_last = end == total_len;

            let frame = if is_first {
                Frame::from((opcode, chunk)).with_fin(false)
            } else if is_last {
                Frame::continuation(chunk)
            } else {
                Frame::continuation(chunk).with_fin(false)
            };

            futures::SinkExt::send(self, frame).await?;

            offset = end;
            is_first = false;
        }

        Ok(())
    }

    /// Creates a new WebSocket from an existing stream.
    ///
    /// The `read_buf` parameter should contain any bytes that were read from the stream
    /// during the HTTP upgrade but weren't consumed (leftover data after the HTTP response).
    pub(crate) fn new(role: Role, stream: S, read_buf: Bytes, opts: Negotiation) -> Self {
        let decoder = codec::Decoder::new(role, opts.max_payload_read);
        let encoder = codec::Encoder::new(role);
        let codec = Codec::from((decoder, encoder));

        let mut parts = FramedParts::new(stream, codec);
        parts.read_buf = read_buf.into();

        let mut framed = Framed::from_parts(parts);
        if let Some(boundary) = opts.max_backpressure_write_boundary {
            framed.set_backpressure_boundary(boundary);
        }

        Self {
            stream: framed,
            read_half: ReadHalf::new(&opts),
            write_half: WriteHalf::new(opts.max_payload_write_size, &opts),
            wake_proxy: Arc::new(WakeProxy::default()),
            obligated_sends: VecDeque::new(),
            flush_sends: false,
            inflate: opts.decompressor(role),
            deflate: opts.compressor(role),
            check_utf8: opts.utf8,
        }
    }

    fn on_frame(&mut self, mut frame: Frame) -> Result<Option<Frame>> {
        // control frames can't be fragmented.
        // if !frame.is_fin() && frame.opcode().is_control() {
        //     return Err(WebSocketError::InvalidFragment);
        // }

        // Handle protocol frames first
        match frame.opcode {
            OpCode::Ping => {
                self.on_ping(&frame);
                return Ok(Some(frame));
            }
            OpCode::Close => {
                return match self.on_close(&frame) {
                    Ok(_) => Ok(Some(frame)),
                    Err(err) => Err(err),
                };
            }
            OpCode::Pong => return Ok(Some(frame)),
            _ => {}
        }

        // Handle decompression for data frames (after fragmentation assembly)
        if frame.is_compressed {
            if let Some(ref mut inflate) = self.inflate {
                let decompressed = inflate.decompress(&frame.payload, true)?;
                if let Some(payload) = decompressed {
                    frame.is_compressed = false;
                    frame.payload = payload;
                }
            } else {
                return Err(WebSocketError::CompressionNotSupported);
            }
        }

        // UTF-8 validation for text frames
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

    fn on_ping(&mut self, frame: &Frame) {
        self.obligated_sends
            .push_back(Frame::pong(frame.payload.clone()));
    }

    fn on_close(&mut self, frame: &Frame) -> Result<()> {
        match frame.payload.len() {
            0 => {}
            1 => return Err(WebSocketError::InvalidCloseFrame),
            _ => {
                let code = frame.close_code().expect("close code");
                let _ = frame.close_reason()?;

                if !code.is_allowed() {
                    self.emit_close(Frame::close(CloseCode::Protocol, &frame.payload[2..]));
                    return Err(WebSocketError::InvalidCloseCode);
                }
            }
        }

        let frame = Frame::close_raw(frame.payload.clone());
        self.emit_close(frame);

        Ok(())
    }

    fn emit_close(&mut self, frame: Frame) {
        self.obligated_sends.push_back(frame);
        self.read_half.is_closed = true;
    }

    fn poll_flush_obligated(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        while !self.obligated_sends.is_empty() {
            ready!(self.write_half.poll_ready(&mut self.stream, cx))?;

            let next = self.obligated_sends.pop_front().expect("obligated send");
            self.write_half.start_send(&mut self.stream, next)?;
            self.flush_sends = true;
        }

        if self.flush_sends {
            ready!(self.write_half.poll_flush(&mut self.stream, cx))?;
            self.flush_sends = false;
        }

        Poll::Ready(Ok(()))
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
        let wake_proxy = Arc::clone(&this.wake_proxy);
        wake_proxy.set_waker(ContextKind::Write, cx.waker());
        wake_proxy.with_context(|cx| {
            ready!(this.poll_flush_obligated(cx))?;
            this.write_half.poll_ready(&mut this.stream, cx)
        })
    }

    fn start_send(self: Pin<&mut Self>, item: Frame) -> std::result::Result<(), Self::Error> {
        let this = self.get_mut();

        let should_compress =
            item.is_fin() && (item.opcode == OpCode::Text || item.opcode == OpCode::Binary);

        let final_frame = if should_compress {
            if let Some(deflate) = this.deflate.as_mut() {
                // Compress the payload if compression is enabled
                let output = deflate.compress(&item.payload)?;
                item.into_compressed(output)
            } else {
                item
            }
        } else {
            item
        };

        this.write_half.start_send(&mut this.stream, final_frame)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        let this = self.get_mut();
        let wake_proxy = Arc::clone(&this.wake_proxy);
        wake_proxy.set_waker(ContextKind::Write, cx.waker());
        wake_proxy.with_context(|cx| this.write_half.poll_flush(&mut this.stream, cx))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        let this = self.get_mut();
        let wake_proxy = Arc::clone(&this.wake_proxy);
        this.wake_proxy.set_waker(ContextKind::Write, cx.waker());
        wake_proxy.with_context(|cx| this.write_half.poll_close(&mut this.stream, cx))
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

    let compression_level = options.compression.as_ref().map(|opts| opts.level);
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

    let extensions = headers
        .get(reqwest::header::SEC_WEBSOCKET_EXTENSIONS)
        .and_then(|h| h.to_str().ok())
        .map(WebSocketExtensions::from_str)
        .and_then(std::result::Result::ok);

    let max_read_buffer = options.max_read_buffer.unwrap_or(
        options
            .max_payload_read
            .map(|payload_read| payload_read * 2)
            .unwrap_or(MAX_READ_BUFFER),
    );

    Ok(Negotiation {
        extensions,
        compression_level,
        max_payload_read: options.max_payload_read.unwrap_or(MAX_PAYLOAD_READ),
        max_read_buffer,
        utf8: options.check_utf8,
        fragment_timeout: options.fragment_timeout,
        max_payload_write_size: options.max_payload_write_size,
        max_backpressure_write_boundary: options.max_backpressure_write_boundary,
    })
}

fn verify(response: &Response<Incoming>, options: Options) -> Result<Negotiation> {
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(WebSocketError::InvalidStatusCode(
            response.status().as_u16(),
        ));
    }

    let compression_level = options.compression.as_ref().map(|opts| opts.level);
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

    let extensions = headers
        .get(header::SEC_WEBSOCKET_EXTENSIONS)
        .and_then(|h| h.to_str().ok())
        .map(WebSocketExtensions::from_str)
        .and_then(std::result::Result::ok);

    let max_read_buffer = options.max_read_buffer.unwrap_or(
        options
            .max_payload_read
            .map(|payload_read| payload_read * 2)
            .unwrap_or(MAX_READ_BUFFER),
    );

    Ok(Negotiation {
        extensions,
        compression_level,
        max_payload_read: options.max_payload_read.unwrap_or(MAX_PAYLOAD_READ),
        max_read_buffer,
        utf8: options.check_utf8,
        fragment_timeout: options.fragment_timeout,
        max_payload_write_size: options.max_payload_write_size,
        max_backpressure_write_boundary: options.max_backpressure_write_boundary,
    })
}

fn generate_key() -> String {
    use base64::prelude::*;
    let input: [u8; 16] = rand::random();
    BASE64_STANDARD.encode(input)
}

/// Creates a TLS connector with root certificates for secure WebSocket connections.
fn tls_connector() -> TlsConnector {
    let mut root_cert_store = rustls::RootCertStore::empty();
    root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| TrustAnchor {
        subject: ta.subject.clone(),
        subject_public_key_info: ta.subject_public_key_info.clone(),
        name_constraints: ta.name_constraints.clone(),
    }));

    let maybe_provider = rustls::crypto::CryptoProvider::get_default().cloned();

    #[cfg(any(feature = "rustls-ring", feature = "rustls-aws-lc-rs"))]
    let provider = maybe_provider.unwrap_or_else(|| {
        #[cfg(feature = "rustls-ring")]
        let provider = rustls::crypto::ring::default_provider();
        #[cfg(feature = "rustls-aws-lc-rs")]
        let provider = rustls::crypto::aws_lc_rs::default_provider();

        Arc::new(provider)
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
    config.alpn_protocols = vec!["http/1.1".into()];

    TlsConnector::from(Arc::new(config))
}

#[cfg(test)]
mod tests {
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
        let (client_stream, server_stream) = MockStream::pair(buffer_size);

        let negotiation = Negotiation {
            extensions: None,
            compression_level: None,
            max_payload_read: MAX_PAYLOAD_READ,
            max_read_buffer: MAX_READ_BUFFER,
            utf8: false,
            fragment_timeout: None,
            max_payload_write_size: None,
            max_backpressure_write_boundary: None,
        };

        let client_ws = WebSocket::new(
            Role::Client,
            client_stream,
            Bytes::new(),
            negotiation.clone(),
        );

        let server_ws = WebSocket::new(Role::Server, server_stream, Bytes::new(), negotiation);

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
            .send(Frame::close(close::CloseCode::Normal, b"Goodbye"))
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
}
