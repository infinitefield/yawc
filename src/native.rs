use crate::{close, codec, compression, frame, stream, Result, WebSocketError};

use {
    bytes::Bytes,
    http_body_util::Empty,
    hyper::{body::Incoming, header, upgrade::Upgraded, Request, Response, StatusCode},
    hyper_util::rt::TokioIo,
    pin_project::pin_project,
    sha1::{Digest, Sha1},
    stream::MaybeTlsStream,
    tokio::net::TcpStream,
    tokio_rustls::{rustls::pki_types::ServerName, TlsConnector},
};

use std::{
    collections::VecDeque,
    future::{poll_fn, Future},
    io,
    net::SocketAddr,
    pin::{pin, Pin},
    str::FromStr,
    sync::Arc,
    task::{ready, Context, Poll},
};

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncWrite};

use close::CloseCode;
use codec::Codec;
use compression::{Compressor, Decompressor, WebSocketExtensions};
use futures::{future::BoxFuture, task::AtomicWaker, FutureExt, SinkExt, StreamExt};
use tokio_rustls::rustls::{self, pki_types::TrustAnchor};
use tokio_util::codec::Framed;
use url::Url;

pub use frame::{Frame, FrameView, OpCode};

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

/// Type alias for the compression level used in WebSocket compression settings.
///
/// This alias refers to `flate2::Compression`, allowing easy configuration of compression levels
/// when creating compressors and decompressors for WebSocket frames.
pub type CompressionLevel = flate2::Compression;

/// Type alias for HTTP requests used in WebSocket connection handling.
///
/// This alias represents HTTP requests with an empty body, used primarily for
/// WebSocket protocol negotiation during the initial handshake process. It encapsulates
/// the HTTP headers and metadata necessary for establishing WebSocket connections
/// according to RFC 6455, while maintaining a minimal memory footprint by using
/// an empty body type.
///
/// Used in conjunction with WebSocket upgrade mechanics to parse and validate
/// incoming connection requests before transitioning to the WebSocket protocol.
pub type HttpRequest = hyper::http::request::Request<()>;

/// Type alias for HTTP request builders used in WebSocket client connection setup.
///
/// This alias represents the builder pattern used to construct HTTP requests during
/// WebSocket handshake initialization. It encapsulates the ability to set headers,
/// method, URI, and other request properties required for proper WebSocket protocol
/// negotiation according to RFC 6455.
///
/// Used primarily in the client-side connection process to prepare the initial
/// HTTP upgrade request with the appropriate WebSocket-specific headers.
pub type HttpRequestBuilder = hyper::http::request::Builder;

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
struct Negotitation {
    extensions: Option<WebSocketExtensions>,
    compression_level: Option<CompressionLevel>,
    max_payload_read: usize,
    max_read_buffer: usize,
    utf8: bool,
}

impl Negotitation {
    pub(crate) fn decompressor(&self, role: Role) -> Option<compression::Decompressor> {
        let config = self.extensions.as_ref()?;

        #[cfg(feature = "logging")]
        log::debug!(
            "Established decompressor for {role} with settings \
            client_no_context_takeover={} server_no_context_takeover={}",
            config.client_no_context_takeover,
            config.client_no_context_takeover
        );

        // configure the decompressor using the assigned role and preferred flags.
        Some(if role == Role::Server {
            if config.client_no_context_takeover {
                compression::Decompressor::no_context_takeover()
            } else {
                // zlib
                #[cfg(feature = "zlib")]
                if config.client_max_window_bits.is_some() {
                    let window_bits = config.client_max_window_bits.unwrap();
                    compression::Decompressor::new_with_window_bits(window_bits)
                } else {
                    compression::Decompressor::new()
                }
                #[cfg(not(feature = "zlib"))]
                compression::Decompressor::new()
            }
        } else {
            // client
            if config.server_no_context_takeover {
                compression::Decompressor::no_context_takeover()
            } else {
                #[cfg(feature = "zlib")]
                if config.server_max_window_bits.is_some() {
                    let window_bits = config.server_max_window_bits.unwrap();
                    compression::Decompressor::new_with_window_bits(window_bits)
                } else {
                    compression::Decompressor::new()
                }
                #[cfg(not(feature = "zlib"))]
                compression::Decompressor::new()
            }
        })
    }

    pub(crate) fn compressor(&self, role: Role) -> Option<compression::Compressor> {
        let config = self.extensions.as_ref()?;

        #[cfg(feature = "logging")]
        log::debug!(
            "Established compressor for {role} with settings \
            client_no_context_takeover={} server_no_context_takeover={}",
            config.client_no_context_takeover,
            config.client_no_context_takeover
        );

        let level = self.compression_level.unwrap();

        // configure the compressor using the assigned role and preferred flags.
        Some(if role == Role::Client {
            if config.client_no_context_takeover {
                compression::Compressor::no_context_takeover(level)
            } else {
                // server
                #[cfg(feature = "zlib")]
                if config.client_max_window_bits.is_some() {
                    let window_bits = config.client_max_window_bits.unwrap();
                    compression::Compressor::new_with_window_bits(level, window_bits)
                } else {
                    compression::Compressor::new(level)
                }
                #[cfg(not(feature = "zlib"))]
                compression::Compressor::new(level)
            }
        } else {
            // server
            if config.server_no_context_takeover {
                compression::Compressor::no_context_takeover(level)
            } else {
                // zlib
                #[cfg(feature = "zlib")]
                if config.server_max_window_bits.is_some() {
                    let window_bits = config.server_max_window_bits.unwrap();
                    compression::Compressor::new_with_window_bits(level, window_bits)
                } else {
                    compression::Compressor::new(level)
                }
                #[cfg(not(feature = "zlib"))]
                compression::Compressor::new(level)
            }
        })
    }
}

/// The role the wwebsocket stream is taking.
///
/// When a server role is taken the frames will not be masked, unlike
/// the client role, in which frames are masked.
#[derive(Copy, Clone, PartialEq)]
enum Role {
    Server,
    Client,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server => {
                write!(f, "server")
            }
            Self::Client => {
                write!(f, "client")
            }
        }
    }
}

/// Represents an incoming WebSocket upgrade request that can be converted into a WebSocket connection.
///
/// Available when the `axum` feature is enabled, this struct integrates with the axum web framework
/// to handle WebSocket upgrades in axum route handlers.
///
/// # Example
/// ```rust
/// use axum::{
///     routing::get,
///     response::IntoResponse,
/// };
/// use yawc::{IncomingUpgrade, CompressionLevel, Options};
///
/// struct AppState;
///
/// async fn ws_handler(
///     ws: IncomingUpgrade,
///     state: AppState,
/// ) -> impl IntoResponse {
///     let options = Options::default()
///         .with_compression_level(CompressionLevel::best());
///
///     let (response, fut) = ws.upgrade(options).unwrap();
///
///     // Spawn handler for upgraded WebSocket connection
///     tokio::spawn(async move {
///         let ws = fut.await.unwrap();
///         // Handle WebSocket connection
///     });
///
///     response
/// }
/// ```
///
/// # Compression Support
/// Supports per-message compression via the `permessage-deflate` extension when enabled.
/// The compression settings can be configured through `Options` during upgrade.
///
/// # Protocol
/// - Handles `Sec-WebSocket-Key` validation
/// - Manages protocol switching from HTTP to WebSocket
/// - Negotiates extensions like compression
/// - Returns response headers for upgrade handshake
#[cfg_attr(docsrs, doc(cfg(feature = "axum")))]
#[cfg(feature = "axum")]
pub struct IncomingUpgrade {
    /// The Sec-WebSocket-Key header value from the client. This value is used for the WebSocket
    /// handshake as required by RFC 6455.
    key: String,

    /// The Hyper upgrade future used to complete the protocol switch from HTTP to WebSocket.
    /// This handles the low-level protocol transition after handshake is complete.
    on_upgrade: hyper::upgrade::OnUpgrade,

    /// Optional WebSocket extensions negotiated with the client, such as permessage-deflate compression.
    /// These configure protocol-level behavior if enabled.
    extensions: Option<WebSocketExtensions>,
}

#[cfg_attr(docsrs, doc(cfg(feature = "axum")))]
#[cfg(feature = "axum")]
impl IncomingUpgrade {
    /// Upgrades an HTTP request to a WebSocket connection with the given options
    ///
    /// Creates an HTTP response for the upgrade and constructs the upgrade future
    /// that will complete the protocol switch. Handles negotiation of extensions
    /// like compression between client and server capabilities.
    ///
    /// # Parameters
    /// - `options`: Configuration options for the WebSocket connection like compression settings
    ///
    /// # Returns
    /// A tuple containing:
    /// - The HTTP upgrade response to send to the client
    /// - An upgrade future that completes into a WebSocket connection
    ///
    /// # Examples
    /// ```rust
    /// use axum::{response::IntoResponse, extract::State};
    /// use yawc::{IncomingUpgrade, Options};
    ///
    /// async fn handler(
    ///     ws: IncomingUpgrade,
    ///     State(state): State<()>,
    /// ) -> impl IntoResponse {
    ///     let options = Options::default();
    ///     let (response, upgrade) = ws.upgrade(options).unwrap();
    ///
    ///     tokio::spawn(async move {
    ///         let websocket = upgrade.await.unwrap();
    ///         // Handle WebSocket connection
    ///     });
    ///
    ///     response
    /// }
    /// ```
    ///
    /// # Protocol Details
    /// The upgrade process:
    /// 1. Creates upgrade response with standard WebSocket headers
    /// 2. Negotiates any extensions like compression
    /// 3. Returns response to be sent to client
    /// 4. UpgradeFut completes after response sent
    ///
    /// When compression is enabled:
    /// - Client and server negotiate compression settings
    /// - Per-message compression parameters are synchronized
    /// - Compression context is initialized after upgrade
    pub fn upgrade(self, options: Options) -> Result<(Response<Empty<Bytes>>, UpgradeFut)> {
        let builder = Response::builder()
            .status(hyper::StatusCode::SWITCHING_PROTOCOLS)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_ACCEPT, self.key);

        let (builder, extensions) = match (self.extensions, options.compression.as_ref()) {
            (Some(client_offer), Some(server_offer)) => {
                let offer = server_offer.merge(&client_offer);
                let response = builder.header(header::SEC_WEBSOCKET_EXTENSIONS, offer.to_string());
                (response, Some(offer))
            }
            _ => (builder, None),
        };

        let response = builder
            .body(Empty::new())
            .expect("bug: failed to build response");

        // max read buffer should be at least 2 times the payload read if not specified
        let max_read_buffer = options.max_read_buffer.unwrap_or(
            options
                .max_payload_read
                .map(|payload_read| payload_read * 2)
                .unwrap_or(MAX_READ_BUFFER),
        );

        let stream = UpgradeFut {
            inner: self.on_upgrade,
            negotiation: Some(Negotitation {
                extensions,
                compression_level: options
                    .compression
                    .as_ref()
                    .map(|compression| compression.level),
                max_payload_read: options.max_payload_read.unwrap_or(MAX_PAYLOAD_READ),
                max_read_buffer,
                utf8: options.check_utf8,
            }),
        };

        Ok((response, stream))
    }
}

/// Implementation of extracting a WebSocket upgrade from an Axum request.
///
/// This implementation allows the `IncomingUpgrade` to be used as an extractor in axum route handlers,
/// performing the necessary validations and setup for a WebSocket upgrade. It verifies required headers
/// like `Sec-WebSocket-Key` and `Sec-WebSocket-Version`, and extracts any offered WebSocket extensions.
///
/// # Errors
/// Returns `StatusCode::BAD_REQUEST` if:
/// - The `Sec-WebSocket-Key` header is missing
/// - The `Sec-WebSocket-Version` is not "13"
/// - The request is not an upgrade request
///
/// # Example
/// ```rust
/// use axum::routing::get;
/// use axum::response::IntoResponse;
/// use yawc::IncomingUpgrade;
///
/// async fn ws_upgrade(ws: yawc::IncomingUpgrade) -> impl IntoResponse {
///     // handle
/// }
///
/// let app: axum::Router<()> = axum::Router::new()
///     .route("/ws", get(ws_upgrade));
/// ```
#[cfg(feature = "axum")]
#[cfg_attr(docsrs, doc(cfg(feature = "axum")))]
#[async_trait::async_trait]
impl<S> axum_core::extract::FromRequestParts<S> for IncomingUpgrade
where
    S: Sync,
{
    type Rejection = hyper::StatusCode;

    /// Extracts WebSocket upgrade parameters from HTTP request parts
    ///
    /// Validates WebSocket protocol requirements and extracts:
    /// - The client's Sec-WebSocket-Key
    /// - The protocol version (must be 13)
    /// - Any extension offers from the client
    /// - The upgrade future from Hyper
    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        let key = parts
            .headers
            .get(header::SEC_WEBSOCKET_KEY)
            .ok_or(http::StatusCode::BAD_REQUEST)?;

        if parts
            .headers
            .get(header::SEC_WEBSOCKET_VERSION)
            .map(|v| v.as_bytes())
            != Some(b"13")
        {
            return Err(hyper::StatusCode::BAD_REQUEST);
        }

        let extensions = parts
            .headers
            .get(header::SEC_WEBSOCKET_EXTENSIONS)
            .and_then(|h| h.to_str().ok())
            .map(WebSocketExtensions::from_str)
            .and_then(std::result::Result::ok);

        let on_upgrade = parts
            .extensions
            .remove::<hyper::upgrade::OnUpgrade>()
            .ok_or(hyper::StatusCode::BAD_REQUEST)?;

        Ok(Self {
            on_upgrade,
            extensions,
            key: sec_websocket_protocol(key.as_bytes()),
        })
    }
}

fn sec_websocket_protocol(key: &[u8]) -> String {
    use base64::prelude::*;
    let mut sha1 = Sha1::new();
    sha1.update(key);
    sha1.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"); // magic string
    let result = sha1.finalize();
    BASE64_STANDARD.encode(&result[..])
}

/// Future that completes the WebSocket upgrade process on a server, returning a WebSocket stream.
///
/// This future is returned by the [`WebSocket::upgrade`] and [`WebSocket::upgrade_with_options`] functions after initiating
/// a WebSocket protocol upgrade. It manages completion of the HTTP upgrade handshake and initializes
/// a WebSocket connection with the negotiated parameters.
///
/// # Important
/// The associated HTTP upgrade response must be sent to the client before polling this future.
/// The future will not complete until the response is sent and the HTTP connection is upgraded
/// to the WebSocket protocol.
///
/// # Example
/// ```no_run
/// use hyper::{
///     body::{Bytes, Incoming},
///     server::conn::http1,
///     service::service_fn,
///     Request, Response,
/// };
/// use http_body_util::Empty;
/// use yawc::{Result, WebSocket, UpgradeFut, Options};
///
/// async fn handle_client(fut: UpgradeFut) -> yawc::Result<()> {
///     let ws = fut.await?;
///     // use `ws`
///     Ok(())
/// }
///
/// async fn server_upgrade(mut req: Request<Incoming>) -> yawc::Result<Response<Empty<Bytes>>> {
///     let (response, fut) = WebSocket::upgrade_with_options(
///         &mut req,
///         Options::default()
///     )?;
///
///     tokio::task::spawn(async move {
///         if let Err(e) = handle_client(fut).await {
///             eprintln!("Error in websocket connection: {}", e);
///         }
///     });
///
///     Ok(response)
/// }
/// ```
///
/// # Fields
/// - `inner`: The underlying hyper upgrade future that completes the protocol switch
/// - `negotiation`: Parameters negotiated during the upgrade, like compression settings
#[pin_project]
#[derive(Debug)]
pub struct UpgradeFut {
    #[pin]
    inner: hyper::upgrade::OnUpgrade,
    negotiation: Option<Negotitation>,
}

impl std::future::Future for UpgradeFut {
    type Output = hyper::Result<WebSocket>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.project();
        let upgraded = match this.inner.poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(x) => x,
        };

        let io = TokioIo::new(upgraded?);
        let negotiation = this.negotiation.take().unwrap();

        Poll::Ready(Ok(WebSocket::new(
            Role::Server,
            HttpStream::from(io),
            negotiation,
        )))
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
///
/// The WakeProxy acts as a task notification manager for asynchronous I/O operations in the WebSocket.
/// It maintains two independent wakers - one for read operations and one for write operations. This
/// separation is crucial for enabling true concurrent I/O:
///
/// - When data arrives to be read, only tasks waiting to read are woken
/// - When the stream becomes writable, only tasks waiting to write are woken
/// - Tasks don't unnecessarily wake each other, improving efficiency
///
/// This design enables full-duplex communication where reads and writes can happen simultaneously
/// without blocking each other. The atomic nature of the wakers ensures thread-safety when multiple
/// tasks are interacting with the WebSocket.
///
/// Key benefits:
/// - Prevents deadlocks that could occur if read/write operations shared a single waker
/// - Enables efficient multiplexing of I/O tasks
/// - Provides clean separation of concerns between reading and writing
/// - Ensures thread-safe wake-up notifications in concurrent scenarios
///
/// When used with `StreamExt::split()`, the WakeProxy allows the split halves of the WebSocket
/// to operate independently while maintaining proper synchronization.
///
/// See [tokio-tungstenite implementation](https://github.com/snapview/tokio-tungstenite/blob/015e00d9ccb447161ab69f18946d501c71d0f689/src/compat.rs#L21)
#[derive(Default)]
struct WakeProxy {
    /// Waker for read operations
    read_waker: AtomicWaker,
    /// Waker for write operations
    write_waker: AtomicWaker,
}

impl futures::task::ArcWake for WakeProxy {
    /// Wakes both read and write contexts when the WebSocket needs attention.
    ///
    /// This implementation ensures that both read and write operations are notified
    /// when the WebSocket state changes, allowing proper handling of bi-directional
    /// communication. The independent waking of read and write contexts enables
    /// concurrent operation without blocking either direction.
    fn wake_by_ref(this: &Arc<Self>) {
        this.read_waker.wake();
        this.write_waker.wake();
    }
}

impl WakeProxy {
    /// Registers a waker for either read or write operations.
    ///
    /// # Parameters
    /// - `kind`: Specifies whether this is for a read or write context
    /// - `waker`: The waker to register for the specified operation type
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

    /// Executes a closure with a new task context created from this demultiplexer.
    ///
    /// # Parameters
    /// - `f`: Closure to execute with the created context
    ///
    /// # Returns
    /// Returns the result of executing the closure with the created context.
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
///
/// This enum allows the WebSocket implementation to work with different HTTP client libraries:
/// - With the `reqwest` feature: Uses `reqwest::Upgraded` for client connections
pub enum HttpStream {
    /// The reqwest-based WebSocket stream, available when the `reqwest` feature is enabled
    #[cfg(feature = "reqwest")]
    Reqwest(reqwest::Upgraded),
    /// The hyper-based WebSocket stream, available when the `hyper` feature is enabled
    Hyper(TokioIo<Upgraded>),
}

/// Implements conversion from hyper's `TokioIo<Upgraded>` to `WebSocketStream`
impl From<TokioIo<Upgraded>> for HttpStream {
    fn from(value: TokioIo<Upgraded>) -> Self {
        Self::Hyper(value)
    }
}

/// Implements conversion from reqwest's `Upgraded` to `WebSocketStream`
#[cfg(feature = "reqwest")]
impl From<reqwest::Upgraded> for HttpStream {
    fn from(value: reqwest::Upgraded) -> Self {
        Self::Reqwest(value)
    }
}

/// Implements asynchronous reading for WebSocketStream
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

/// Implements asynchronous writing for WebSocketStream
impl AsyncWrite for HttpStream {
    /// Attempts to write bytes from `buf` into the stream
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

    /// Attempts to flush the stream, ensuring all intermediately buffered contents reach their destination
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

    /// Initiates or attempts to shut down the stream
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

/// Configuration options for a WebSocket connection.
///
/// `Options` allows users to set parameters that govern the behavior of a WebSocket connection,
/// including payload size limits, compression settings, and UTF-8 validation requirements.
#[derive(Clone, Default)]
pub struct Options {
    /// Maximum allowed payload size for incoming messages, in bytes.
    ///
    /// If a message exceeds this size, the connection will be closed immediately
    /// to prevent overloading the receiving end.
    ///
    /// Default: 1 MiB (1,048,576 bytes) as defined in [`MAX_PAYLOAD_READ`]
    pub max_payload_read: Option<usize>,

    /// Maximum size allowed for the buffer that accumulates fragmented messages.
    ///
    /// WebSocket messages can be split into multiple fragments for transmission. These fragments
    /// are accumulated in a read buffer until the complete message is received. To prevent memory
    /// exhaustion attacks using extremely large fragmented messages, once the total size of
    /// accumulated fragments exceeds this limit, the connection is closed.
    ///
    /// Default: 2 MiB (2,097,152 bytes) as defined in [`MAX_READ_BUFFER`], or twice the
    /// configured `max_payload_read` value if that is set.
    pub max_read_buffer: Option<usize>,

    /// Compression settings for the WebSocket connection.
    ///
    /// Compression is based on the Deflate algorithm, with additional configuration available
    /// through [`DeflateOptions`] for finer control over the compression strategy.
    pub compression: Option<DeflateOptions>,

    /// Flag to determine whether incoming messages should be validated for UTF-8 encoding.
    ///
    /// If `true`, the [`ReadHalf`] will validate that received text frames contain valid UTF-8
    /// data, closing the connection on any validation failure.
    ///
    /// Default: `false`
    pub check_utf8: bool,
}

/// Configuration options for WebSocket message compression using the Deflate algorithm.
///
/// The WebSocket protocol supports per-message compression using a Deflate-based algorithm
/// (RFC 7692) to reduce bandwidth usage. This struct allows fine-grained control over
/// compression behavior and memory usage through several key settings:
///
/// # Compression Level
/// Controls the tradeoff between compression ratio and CPU usage via the `level` field.
/// Higher levels provide better compression but require more processing time.
///
/// # Context Management
/// Offers two modes for managing the compression context:
///
/// - **Context Takeover** (default): Maintains compression state between messages,
///   providing better compression ratios at the cost of increased memory usage.
///   Ideal for applications prioritizing bandwidth efficiency.
///
/// - **No Context Takeover**: Resets compression state after each message,
///   reducing memory usage at the expense of compression efficiency.
///   Better suited for memory-constrained environments.
///
/// # Memory Window Size
/// When the `zlib` feature is enabled, allows precise control over the compression
/// window size for both client and server, enabling further optimization of the
/// memory-compression tradeoff.
///
/// # Example
/// ```
/// use yawc::{DeflateOptions, CompressionLevel};
///
/// let opts = DeflateOptions {
///     level: CompressionLevel::default(),
///     server_no_context_takeover: true,
///     ..Default::default()
/// };
/// ```
#[derive(Clone, Default)]
pub struct DeflateOptions {
    /// Sets the compression level (0-9), balancing compression ratio against CPU usage.
    ///
    /// - 0: No compression (fastest)
    /// - 1-3: Low compression (fast)
    /// - 4-6: Medium compression (default)
    /// - 7-9: High compression (slow)
    pub level: CompressionLevel,

    /// Controls the compression window size (in bits) for server-side compression.
    ///
    /// Larger windows improve compression but use more memory. Available only with
    /// the `zlib` feature enabled. Valid range: 8-15 bits.
    #[cfg(feature = "zlib")]
    pub server_max_window_bits: Option<u8>,

    /// Controls the compression window size (in bits) for client-side compression.
    ///
    /// Larger windows improve compression but use more memory. Available only with
    /// the `zlib` feature enabled. Valid range: 8-15 bits.
    #[cfg(feature = "zlib")]
    pub client_max_window_bits: Option<u8>,

    /// Controls server-side compression context management.
    ///
    /// When `true`, compression state is reset after each message, reducing
    /// memory usage at the cost of compression efficiency.
    pub server_no_context_takeover: bool,

    /// Controls client-side compression context management.
    ///
    /// When `true`, compression state is reset after each message, reducing
    /// memory usage at the cost of compression efficiency.
    pub client_no_context_takeover: bool,
}

impl DeflateOptions {
    fn merge(&self, offered: &WebSocketExtensions) -> WebSocketExtensions {
        WebSocketExtensions {
            // Accept client's no_context_takeover settings
            client_no_context_takeover: offered.client_no_context_takeover
                || self.client_no_context_takeover,
            server_no_context_takeover: offered.server_no_context_takeover
                || self.server_no_context_takeover,
            // For window bits, take the minimum of what client offers and what server supports
            #[cfg(feature = "zlib")]
            client_max_window_bits: match (
                offered.client_max_window_bits,
                self.client_max_window_bits,
            ) {
                (Some(c), Some(s)) => Some(c.min(s)),
                (Some(c), None) => Some(c),
                (None, s) => s,
            },
            #[cfg(feature = "zlib")]
            server_max_window_bits: match (
                offered.server_max_window_bits,
                self.server_max_window_bits,
            ) {
                (Some(c), Some(s)) => Some(c.min(s)),
                (Some(c), None) => Some(c),
                (None, s) => s,
            },
            #[cfg(not(feature = "zlib"))]
            client_max_window_bits: None,
            #[cfg(not(feature = "zlib"))]
            server_max_window_bits: None,
        }
    }
}

impl Options {
    /// Sets the compression level for outgoing messages.
    ///
    /// Adjusts the compression level used for message transmission, allowing a balance between
    /// compression efficiency and CPU usage. This option is useful for controlling bandwidth
    /// while managing resource consumption.
    ///
    /// The WebSocket protocol supports two main compression approaches:
    /// - Per-message compression (RFC 7692) using the permessage-deflate extension
    /// - Context takeover, which maintains compression state between messages
    ///
    /// Compression levels range from 0-9:
    /// - 0: No compression (fastest)
    /// - 1-3: Low compression, minimal CPU usage
    /// - 4-6: Balanced compression/CPU trade-off
    /// - 7-9: Maximum compression, highest CPU usage
    ///
    /// For real-time applications with latency constraints, lower compression levels (1-3)
    /// are recommended. For bandwidth-constrained scenarios where CPU usage is less critical,
    /// higher levels (7-9) may be preferable.
    ///
    /// # Parameters
    /// - `level`: The desired compression level, based on the [`CompressionLevel`] type.
    ///
    /// # Returns
    /// A modified `Options` instance with the specified compression level.
    ///
    /// # Example
    /// ```rust
    /// use yawc::{Options, CompressionLevel};
    ///
    /// let options = Options::default()
    ///     .with_compression_level(CompressionLevel::new(6)) // Balanced compression
    ///     .with_utf8(); // Enable UTF-8 validation
    /// ```
    pub fn with_compression_level(self, level: CompressionLevel) -> Self {
        let mut compression = self.compression.unwrap_or_default();
        compression.level = level;

        Self {
            compression: Some(compression),
            ..self
        }
    }

    /// Disables compression for the WebSocket connection.
    ///
    /// Removes any previously configured compression settings, ensuring that
    /// messages sent over this connection will not be compressed. This is useful
    /// when compression would add unnecessary overhead, such as when sending
    /// already-compressed data or small messages.
    ///
    /// # Returns
    /// A modified `Options` instance with compression disabled.
    pub fn without_compression(self) -> Self {
        Self {
            compression: None,
            ..self
        }
    }

    /// Sets the maximum allowed payload size for incoming messages.
    ///
    /// Specifies the maximum size of messages that the WebSocket connection will accept.
    /// If an incoming message exceeds this size, the connection will be terminated to avoid
    /// overloading the receiver.
    ///
    /// # Parameters
    /// - `size`: The maximum payload size in bytes.
    ///
    /// # Returns
    /// A modified `Options` instance with the specified payload size limit.
    pub fn with_max_payload_read(self, size: usize) -> Self {
        Self {
            max_payload_read: Some(size),
            ..self
        }
    }

    /// Sets the maximum read buffer size for accumulated fragmented messages.
    ///
    /// When receiving fragmented WebSocket messages, data is accumulated in a read buffer.
    /// Once this buffer exceeds the specified size limit, it will be reset back to the
    /// initial 8 KiB capacity to prevent unbounded memory growth from very large fragmented
    /// messages.
    ///
    /// # Parameters
    /// - `size`: Maximum size in bytes allowed for the read buffer
    ///
    /// # Returns
    /// A modified `Options` instance with the specified read buffer size limit.
    pub fn with_max_read_buffer(self, size: usize) -> Self {
        Self {
            max_read_buffer: Some(size),
            ..self
        }
    }

    /// Enables UTF-8 validation for incoming text messages.
    ///
    /// When enabled, the WebSocket connection will verify that all received text messages
    /// contain valid UTF-8 data. This is particularly useful for cases where the source of messages
    /// is untrusted, ensuring data integrity.
    ///
    /// # Returns
    /// A modified `Options` instance with UTF-8 validation enabled.
    pub fn with_utf8(self) -> Self {
        Self {
            check_utf8: true,
            ..self
        }
    }

    /// Sets the maximum window size for the client’s decompression (LZ77) window.
    ///
    /// Limits the size of the client’s decompression window, used during message decompression.
    /// This option is available only when compiled with the `zlib` feature.
    ///
    /// # Parameters
    /// - `max_window_bits`: The maximum number of bits for the client decompression window.
    ///
    /// # Returns
    /// A modified `Options` instance with the specified client max window size.
    #[cfg(feature = "zlib")]
    pub fn with_client_max_window_bits(self, max_window_bits: u8) -> Self {
        let mut compression = self.compression.unwrap_or_default();
        compression.client_max_window_bits = Some(max_window_bits);
        Self {
            compression: Some(compression),
            ..self
        }
    }

    /// Disables context takeover for server-side compression.
    ///
    /// In the WebSocket permessage-deflate extension, "context takeover" refers to maintaining
    /// the compression dictionary between messages. With context takeover enabled (default),
    /// the compression algorithm builds and reuses a dictionary of repeated patterns across
    /// multiple messages, potentially achieving better compression ratios for similar data.
    ///
    /// When disabled via this option:
    /// - The compression dictionary is reset after each message
    /// - Memory usage remains constant since the dictionary isn't preserved
    /// - Compression ratio may be lower since patterns can't be reused
    /// - Particularly useful for long-lived connections where memory growth is a concern
    ///
    /// This setting corresponds to the "server_no_context_takeover" extension parameter
    /// in the WebSocket protocol negotiation.
    ///
    /// # Returns
    /// A modified `Options` instance with server context takeover disabled.
    pub fn server_no_context_takeover(self) -> Self {
        let mut compression = self.compression.unwrap_or_default();
        compression.server_no_context_takeover = true;
        Self {
            compression: Some(compression),
            ..self
        }
    }

    /// Disables context takeover for client-side compression.
    ///
    /// In the WebSocket permessage-deflate extension, "context takeover" refers to maintaining
    /// the compression dictionary between messages. The client's compression context is separate
    /// from the server's, allowing asymmetric configuration based on each endpoint's capabilities.
    ///
    /// When disabled via this option:
    /// - The client's compression dictionary is reset after each message
    /// - Client memory usage remains constant since the dictionary isn't preserved
    /// - May reduce compression efficiency but prevents memory growth on clients
    /// - Useful for memory-constrained clients like mobile devices or browsers
    ///
    /// This setting corresponds to the "client_no_context_takeover" extension parameter
    /// in the WebSocket protocol negotiation.
    ///
    /// # Returns
    /// A modified `Options` instance with client context takeover disabled.
    pub fn client_no_context_takeover(self) -> Self {
        let mut compression = self.compression.unwrap_or_default();
        compression.client_no_context_takeover = true;
        Self {
            compression: Some(compression),
            ..self
        }
    }
}

/// Builder for establishing WebSocket connections with customizable options.
///
/// The `WebSocketBuilder` uses a builder pattern to configure a WebSocket connection
/// before establishing it. This allows for flexible configuration of TLS settings,
/// connection options, and HTTP request customization.
///
/// # Example
/// ```no_run
/// use yawc::{WebSocket, Options};
/// use tokio_rustls::TlsConnector;
///
/// async fn connect_example() -> yawc::Result<()> {
///     let ws = WebSocket::connect("wss://example.com/socket".parse()?)
///         .with_options(Options::default().with_utf8())
///         .with_connector(create_tls_connector())
///         .await?;
///
///     // Use the WebSocket
///     Ok(())
/// }
///
/// fn create_tls_connector() -> TlsConnector {
///     // Create a custom TLS connector
///     todo!()
/// }
/// ```
pub struct WebSocketBuilder {
    opts: Option<WsBuilderOpts>,
    future: Option<BoxFuture<'static, Result<WebSocket>>>,
}

/// Internal options structure for WebSocketBuilder.
///
/// Holds all the configuration options needed to establish a WebSocket connection,
/// including the target URL, TLS connector, connection options, and HTTP request builder.
struct WsBuilderOpts {
    url: Url,
    tcp_address: Option<SocketAddr>,
    connector: Option<TlsConnector>,
    establish_options: Option<Options>,
    http_builder: Option<HttpRequestBuilder>,
}

impl WebSocketBuilder {
    /// Creates a new WebSocketBuilder with the specified URL.
    ///
    /// Initializes a builder with default settings that can be customized
    /// before establishing the connection.
    ///
    /// # Parameters
    /// - `url`: The WebSocket URL to connect to (ws:// or wss://)
    fn new(url: Url) -> Self {
        Self {
            opts: Some(WsBuilderOpts {
                url,
                tcp_address: None,
                connector: None,
                establish_options: None,
                http_builder: None,
            }),
            future: None,
        }
    }

    /// Sets a custom TLS connector for secure WebSocket connections.
    ///
    /// This allows for customized TLS settings when connecting to wss:// URLs,
    /// such as custom certificate validation, client certificates, or specific
    /// cipher suites.
    ///
    /// # Parameters
    /// - `connector`: The TLS connector to use for secure connections
    ///
    /// # Returns
    /// The builder for method chaining
    pub fn with_connector(mut self, connector: TlsConnector) -> Self {
        let Some(opts) = &mut self.opts else {
            unreachable!()
        };
        opts.connector = Some(connector);
        self
    }

    /// Sets a custom TCP address for the WebSocket connection.
    ///
    /// This allows connecting to a specific IP address or alternate hostname
    /// rather than resolving the hostname from the URL. This is useful for
    /// testing, connecting through proxies, or when DNS resolution should
    /// be handled differently.
    ///
    /// # Parameters
    /// - `address`: The socket address to connect to
    ///
    /// # Returns
    /// The builder for method chaining
    pub fn with_tcp_address(mut self, address: SocketAddr) -> Self {
        let Some(opts) = &mut self.opts else {
            unreachable!()
        };
        opts.tcp_address = Some(address);
        self
    }

    /// Sets WebSocket connection options.
    ///
    /// Configures settings like compression, maximum payload size, and UTF-8 validation
    /// for the WebSocket connection.
    ///
    /// # Parameters
    /// - `options`: Configuration options for the WebSocket connection
    ///
    /// # Returns
    /// The builder for method chaining
    pub fn with_options(mut self, options: Options) -> Self {
        let Some(opts) = &mut self.opts else {
            unreachable!()
        };
        opts.establish_options = Some(options);
        self
    }

    /// Sets a custom HTTP request builder for the WebSocket handshake.
    ///
    /// Allows customization of the initial HTTP upgrade request, enabling addition
    /// of headers, cookies, or other request properties needed for the connection.
    ///
    /// # Parameters
    /// - `builder`: A custom HTTP request builder for the handshake
    ///
    /// # Returns
    /// The builder for method chaining
    ///
    /// # Example
    /// ```no_run
    /// use yawc::WebSocket;
    ///
    /// async fn connect() -> yawc::Result<()> {
    ///     let ws = WebSocket::connect("wss://example.com/socket".parse()?)
    ///         .with_request(
    ///             yawc::HttpRequestBuilder::new()
    ///                 .header("Host", "custom-host.example.com")
    ///         )
    ///         .await?;
    ///
    ///     // Use WebSocket...
    ///     Ok(())
    /// }
    /// ```
    pub fn with_request(mut self, builder: HttpRequestBuilder) -> Self {
        let Some(opts) = &mut self.opts else {
            unreachable!()
        };
        opts.http_builder = Some(builder);
        self
    }
}

impl Future for WebSocketBuilder {
    type Output = Result<WebSocket>;

    /// Polls the future to establish the WebSocket connection.
    ///
    /// When first called, initializes the connection process with the configured
    /// options. Subsequent calls poll the underlying connection future until
    /// the connection is established or fails.
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(WebSocket))` when connection is successfully established
    /// - `Poll::Ready(Err(_))` when connection fails
    /// - `Poll::Pending` when connection is still in progress
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(opts) = this.opts.take() {
            let future = WebSocket::connect_priv(
                opts.url,
                opts.tcp_address,
                opts.connector,
                opts.establish_options.unwrap_or_default(),
                opts.http_builder.unwrap_or_else(HttpRequest::builder),
            );
            this.future = Some(Box::pin(future));
        }

        let Some(pinned) = &mut this.future else {
            unreachable!()
        };
        pinned.poll_unpin(cx)
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
/// # Connecting
/// To establish a WebSocket connection as a client:
/// ```no_run
/// use tokio::net::TcpStream;
/// use yawc::{WebSocket, frame::OpCode};
/// use futures::StreamExt;
/// use tokio_rustls::TlsConnector;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let ws = WebSocket::connect("wss://echo.websocket.org".parse()?).await?;
///     // Use `ws` for WebSocket communication
///     Ok(())
/// }
/// ```
///
/// # Ping and Pong Handling as a Client
/// When acting as a client, if a `Ping` frame is received from the server, [`WebSocket`]
/// automatically replies with a `Pong` frame. However, the `Ping` frame is still made available
/// to the user before the automatic response:
///
/// ```no_run
/// use tokio::net::TcpStream;
/// use yawc::{WebSocket, frame::OpCode};
/// use futures::StreamExt;
///
/// async fn client_loop(mut ws: WebSocket) -> anyhow::Result<()> {
///     while let Some(frame) = ws.next().await {
///         match frame.opcode {
///             OpCode::Ping => {
///                 let ping_text = std::str::from_utf8(&frame.payload).unwrap();
///                 println!("Received ping with text: {ping_text}");
///                 // The WebSocket automatically sends a Pong in response
///             }
///             _ => {}
///         }
///     }
///
///     Ok(())
/// }
/// ```
///
/// # Splitting the WebSocket
/// For concurrent reading and writing operations, use [`futures::StreamExt::split`] from the futures crate,
/// which maintains all WebSocket protocol handling while enabling simultaneous reads and writes.
/// This is the recommended approach for most concurrent WebSocket operations.
///
/// In contrast, [`WebSocket::split_stream`] is a low-level API that bypasses critical WebSocket
/// protocol management and should rarely be used directly. It disables automatic control frame handling
/// (like Ping/Pong), connection health monitoring, and other protocol-level features. Only use
/// this method if you need direct access to the underlying frame processing and are prepared to
/// handle all protocol requirements manually.
///
/// After calling [`WebSocket::split_stream`], you get direct access to the raw [`ReadHalf`] and
/// [`WriteHalf`] components, as well as the underlying [`HttpStream`] for direct stream access.
/// Read more about their behavior in their respective documentation.
///
pub struct WebSocket {
    stream: Framed<HttpStream, Codec>,
    /// The read half of the WebSocket, responsible for receiving frames.
    read_half: ReadHalf,
    /// The write half of the WebSocket, responsible for sending frames.
    write_half: WriteHalf,
    /// Field sharing wake notifications between overlapping read/write contexts.
    wake_proxy: Arc<WakeProxy>,
    /// Queue for frames that must be sent before closing.
    obligated_sends: VecDeque<FrameView>,
    /// Indicates whether to flush pending frames.
    flush_sends: bool,
    /// Determines if UTF-8 validation is performed on received frames.
    check_utf8: bool,
}

impl WebSocket {
    // ================== Client ====================

    /// Establishes a WebSocket connection to the specified `url`.
    ///
    /// This asynchronous function supports both `ws://` (non-secure) and `wss://` (secure) schemes,
    /// allowing for secure WebSocket connections when needed. For secure connections, a default
    /// TLS configuration will be used.
    ///
    /// # Parameters
    /// - `url`: The WebSocket URL to connect to. This can include both `ws` and `wss` schemes.
    ///   For secure connections, the hostname from this URL is also used as the identity server
    ///   for TLS verification.
    ///
    /// # Returns
    /// A `Result` containing either a connected `WebSocket` instance or an error if the connection could not be established.
    ///
    /// # Examples
    /// ```no_run
    /// use yawc::WebSocket;
    ///
    /// #[tokio::main]
    /// async fn main() -> yawc::Result<()> {
    ///    let ws = WebSocket::connect("wss://echo.websocket.org".parse()?).await?;
    ///    // Use `ws` to send and receive messages
    ///    Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    /// This function will return an error if:
    /// - The URL provided is invalid or not a WebSocket URL.
    /// - A network or TLS error occurs while trying to establish the connection.
    ///
    /// # Notes
    /// - By default, no custom options are applied during the connection process. Use
    ///   `Options::default()` for standard WebSocket behavior.
    /// - For secure connections requiring custom TLS settings, use [`WebSocketBuilder::with_connector()`] instead.
    /// - To connect to a different TCP address than what's specified in the URL, use
    ///   [`WebSocketBuilder::with_tcp_address()`]. This allows connecting to alternative hosts
    ///   while still using the URL's hostname for TLS identity verification.
    pub fn connect(url: Url) -> WebSocketBuilder {
        WebSocketBuilder::new(url)
    }

    async fn connect_priv(
        url: Url,
        tcp_address: Option<SocketAddr>,
        connector: Option<TlsConnector>,
        options: Options,
        builder: HttpRequestBuilder,
    ) -> Result<WebSocket> {
        let host = url.host().expect("hostname").to_string();

        let tcp_stream = if let Some(tcp_address) = tcp_address {
            TcpStream::connect(tcp_address).await?
        } else {
            let port = url.port_or_known_default().expect("port");
            TcpStream::connect(format!("{host}:{port}")).await?
        };

        let stream = match url.scheme() {
            "ws" => MaybeTlsStream::Plain(tcp_stream),
            "wss" => {
                // if the server_name is not defined in the header, fetch it from the url
                let connector = connector.unwrap_or_else(tls_connector);
                let domain = ServerName::try_from(host)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid dnsname"))?;

                MaybeTlsStream::Tls(connector.connect(domain, tcp_stream).await?)
            }
            _ => return Err(WebSocketError::InvalidHttpScheme),
        };

        Self::handshake_with_request(url, stream, options, builder).await
    }

    /// Performs a WebSocket handshake over an existing connection.
    ///
    /// This asynchronous function establishes a WebSocket connection by performing the handshake with
    /// a pre-existing TCP stream or similar I/O type. It can be used when an active connection to a
    /// server exists, bypassing the need to initiate a new connection for the WebSocket.
    ///
    /// # Parameters
    /// - `url`: The WebSocket URL to connect to, which should specify either `ws` or `wss` scheme.
    /// - `io`: An I/O stream that implements `AsyncWrite` and `AsyncRead`, allowing WebSocket communication
    ///         over this stream after the handshake completes. Typically, this is a `TcpStream`.
    /// - `options`: [`Options`] to configure the WebSocket connection, such as compression and ping intervals.
    ///
    /// # Returns
    /// A `Result` containing either an initialized `WebSocket` instance upon successful handshake or an error
    /// if the handshake fails.
    ///
    /// # Examples
    /// ```no_run
    /// use tokio::net::TcpStream;
    /// use yawc::{WebSocket, Result, Options};
    ///
    /// async fn handle_client(
    ///     url: url::Url,
    ///     socket: TcpStream,
    /// ) -> Result<()> {
    ///     let mut ws = WebSocket::handshake(url, socket, Options::default()).await?;
    ///     // Use `ws` to send and receive messages after a successful handshake
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    /// This function may return an error if:
    /// - The URL provided is invalid or does not specify a WebSocket URL.
    /// - The handshake response does not meet WebSocket protocol requirements (e.g., missing required headers).
    /// - A network or I/O error occurs during the handshake process.
    ///
    /// # Notes
    /// - The function handles setting up necessary WebSocket headers, including `Host`, `Upgrade`, `Connection`,
    ///   `Sec-WebSocket-Key`, and `Sec-WebSocket-Version`.
    /// - If compression is enabled via `Options`, it will be negotiated as part of the handshake.
    ///
    /// # Usage
    /// This function is useful when handling WebSocket connections with existing streams, such as when
    /// managing multiple connections in a server context.
    ///
    pub async fn handshake<S>(url: Url, io: S, options: Options) -> Result<WebSocket>
    where
        S: AsyncWrite + AsyncRead + Send + Unpin + 'static,
    {
        Self::handshake_with_request(url, io, options, HttpRequest::builder()).await
    }

    /// Performs a WebSocket handshake with a customizable HTTP request.
    ///
    /// This method extends the basic `handshake` functionality by allowing you to provide
    /// a custom HTTP request builder, which can be used to set additional headers or
    /// customize the request before performing the WebSocket handshake.
    ///
    /// # Parameters
    /// - `url`: The WebSocket URL to connect to
    /// - `io`: The I/O stream to use for the WebSocket connection
    /// - `options`: Configuration options for the WebSocket connection
    /// - `builder`: A custom HTTP request builder for customizing the handshake request
    ///
    /// # Returns
    /// A `Result` containing the established WebSocket connection if successful
    ///
    /// # Example
    /// ```no_run
    /// use yawc::{WebSocket, Options, Result};
    /// use tokio::net::TcpStream;
    ///
    /// async fn connect() -> Result<WebSocket> {
    ///     let stream = TcpStream::connect("example.com:80").await.unwrap();
    ///     let builder = yawc::HttpRequest::builder()
    ///         .header("User-Agent", "My Custom WebSocket Client")
    ///         .header("Authorization", "Bearer token123");
    ///
    ///     WebSocket::handshake_with_request(
    ///         "ws://example.com/socket".parse().unwrap(),
    ///         stream,
    ///         Options::default(),
    ///         builder
    ///     ).await
    /// }
    /// ```
    pub async fn handshake_with_request<S>(
        url: Url,
        io: S,
        options: Options,
        mut builder: HttpRequestBuilder,
    ) -> Result<WebSocket>
    where
        S: AsyncWrite + AsyncRead + Send + Unpin + 'static,
    {
        // allow the user to set a custom Host header.
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

        tokio::spawn(async move {
            if let Err(_err) = conn.with_upgrades().await {
                #[cfg(feature = "logging")]
                log::error!("upgrading connection: {:?}", _err);
            }
        });

        let mut response = sender.send_request(req).await?;
        let negotiated = verify(&response, options)?;

        let upgraded = hyper::upgrade::on(&mut response).await?;
        let stream = TokioIo::new(upgraded);

        Ok(WebSocket::new(
            Role::Client,
            HttpStream::from(stream),
            negotiated,
        ))
    }

    /// Performs a WebSocket handshake when using the `reqwest` HTTP client.
    ///
    /// This function is only available when the `reqwest` feature is enabled. It provides WebSocket
    /// handshake functionality using the `reqwest::Client` for HTTP request handling and connection
    /// management.
    ///
    /// # Parameters
    /// - `url`: The WebSocket URL to connect to, supporting both `ws` and `wss` schemes.
    /// - `client`: A configured `reqwest::Client` instance that will be used to initiate the connection.
    /// - `options`: [`Options`] for configuring connection behavior like compression and payload limits.
    ///
    /// # Returns
    /// A `Result` containing either a connected `WebSocket` instance or an error if the handshake fails.
    ///
    /// # Example
    /// ```no_run
    /// use reqwest::Client;
    /// use yawc::{WebSocket, Options, Result};
    ///
    /// async fn connect_ws(url: String) -> Result<()> {
    ///     let client = Client::new();
    ///     let ws = WebSocket::reqwest(
    ///         url.parse()?,
    ///         client,
    ///         Options::default()
    ///     ).await?;
    ///
    ///     // Use WebSocket for communication
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Notes
    /// - This function requires the `reqwest` feature to be enabled in your Cargo.toml
    /// - Handles WebSocket protocol negotiation including compression if configured
    #[cfg(feature = "reqwest")]
    #[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
    pub async fn reqwest(
        mut url: Url,
        client: reqwest::Client,
        options: Options,
    ) -> Result<WebSocket> {
        let host = url.host().expect("hostname").to_string();
        let port_defined = url.port().is_some();
        let port = url.port_or_known_default().expect("port");

        let host_header = if port_defined {
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
            negotiated,
        ))
    }

    // ================== Server ====================

    // all of this upgrade methods have been mostly copied from fastwebsockets.

    /// Upgrades an HTTP connection to a WebSocket one.
    ///
    /// Validates incoming HTTP request headers and handles protocol switching to establish
    /// a WebSocket connection. Returns a response to be sent back to the client and a future
    /// that will resolve into the WebSocket stream.
    ///
    /// The response must be sent to the client before the upgrade future is awaited. Verification
    /// of headers like Origin, Sec-WebSocket-Protocol and Sec-WebSocket-Extensions is left to
    /// the caller.
    ///
    /// Note: Connection and Upgrade headers must be validated separately. You can use header inspection
    /// to confirm this is a valid WebSocket upgrade request.
    ///
    /// # Parameters
    /// - `request`: The incoming HTTP request to upgrade
    ///
    /// # Returns
    /// Returns a tuple containing:
    /// - Response with SWITCHING_PROTOCOLS status to send back
    /// - UpgradeFut that resolves to the WebSocket stream
    ///
    /// # Example
    /// ```no_run
    /// use hyper::{
    ///     body::{Bytes, Incoming},
    ///     server::conn::http1,
    ///     service::service_fn,
    ///     Request, Response,
    /// };
    /// use yawc::{Result, WebSocket, UpgradeFut, Options};
    ///
    /// async fn handle_client(fut: UpgradeFut) -> yawc::Result<()> {
    ///     let ws = fut.await?;
    ///     Ok(())
    /// }
    ///
    /// async fn server_upgrade(mut req: Request<Incoming>) -> anyhow::Result<yawc::HttpResponse> {
    ///     let (response, fut) = WebSocket::upgrade_with_options(
    ///         &mut req,
    ///         Options::default()
    ///     )?;
    ///
    ///     tokio::task::spawn(async move {
    ///         if let Err(e) = handle_client(fut).await {
    ///             eprintln!("Error in websocket connection: {}", e);
    ///         }
    ///     });
    ///
    ///     Ok(response)
    /// }
    /// ```
    ///
    /// # Errors
    /// Returns error if:
    /// - Sec-WebSocket-Key header is missing
    /// - Sec-WebSocket-Version is not "13"
    ///
    pub fn upgrade<B>(request: impl std::borrow::BorrowMut<Request<B>>) -> UpgradeResult {
        Self::upgrade_with_options(request, Options::default())
    }

    /// Attempts to upgrade an incoming `hyper::Request` to a WebSocket connection with customizable options.
    ///
    /// Similar to [`WebSocket::upgrade`], this function verifies the required WebSocket headers and, if successful,
    /// returns an HTTP response to switch protocols along with an upgrade future for the WebSocket stream.
    /// Additionally, it applies the specified `options`, which may define parameters such as compression settings,
    /// UTF-8 validation, and maximum payload size.
    ///
    /// # Parameters
    /// - `request`: A mutable reference to the HTTP request to upgrade.
    /// - `options`: [`Options`] that define parameters for the WebSocket connection, including compression and payload limits.
    ///
    /// # Returns
    /// A `Result` containing:
    /// - A `Response` with `SWITCHING_PROTOCOLS` status, which should be sent to the client.
    /// - An `UpgradeFut` future that resolves to the WebSocket stream once the response is sent.
    ///
    /// # Notes
    /// - `options.compression`: If enabled and supported by both client and server, the WebSocket connection will use compression.
    /// - `options.max_payload_read`: Specifies the maximum size for received messages.
    /// - The upgrade request must be verified for WebSocket headers before calling this function, as it does not inspect `Connection` or `Upgrade` headers.
    ///
    /// # Example
    /// ```no_run
    /// use hyper::{Request, body::Incoming};
    /// use yawc::{Result, WebSocket, Options};
    ///
    /// async fn handle_websocket_with_options(request: Request<Incoming>) -> Result<()> {
    ///     let options = Options::default();
    ///     let (response, upgrade) = WebSocket::upgrade_with_options(request, options)?;
    ///     // Send `response` back to client, then await `upgrade` for WebSocket stream
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    /// This function returns an error if:
    /// - The `Sec-WebSocket-Key` header is missing.
    /// - The `Sec-WebSocket-Version` header is not set to "13".
    ///
    pub fn upgrade_with_options<B>(
        mut request: impl std::borrow::BorrowMut<Request<B>>,
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
                sec_websocket_protocol(key.as_bytes()),
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

        // max read buffer should be at least 2 times the payload read if not specified
        let max_read_buffer = options.max_read_buffer.unwrap_or(
            options
                .max_payload_read
                .map(|payload_read| payload_read * 2)
                .unwrap_or(MAX_READ_BUFFER),
        );

        let stream = UpgradeFut {
            inner: hyper::upgrade::on(request),
            negotiation: Some(Negotitation {
                extensions,
                compression_level: options
                    .compression
                    .as_ref()
                    .map(|compression| compression.level),
                max_payload_read: options.max_payload_read.unwrap_or(MAX_PAYLOAD_READ),
                max_read_buffer,
                utf8: options.check_utf8,
            }),
        };

        Ok((response, stream))
    }

    // ======== common websocket functions =============

    /// Splits the [`WebSocket`] into its low-level components for advanced usage.
    ///
    /// This method breaks down the WebSocket into three parts:
    /// - `Framed<HttpStream, Codec>`: The raw stream with codec for handling WebSocket frames
    /// - [`ReadHalf`]: Low-level component for receiving and processing frames
    /// - [`WriteHalf`]: Low-level component for sending frames
    ///
    /// # Safety
    /// This function is unsafe because:
    /// - It splits ownership of shared state between components without synchronization
    /// - You must ensure proper coordination between the split halves to maintain protocol correctness
    /// - Misuse of the split components can cause protocol violations or memory corruption
    /// - Manual management of component lifetimes is required to prevent use-after-free
    ///
    /// # Warning
    /// This is a low-level API that should rarely be used directly. For most concurrent read/write
    /// use cases, use [`StreamExt::split`] instead, which preserves the WebSocket's built-in
    /// protocol handling while enabling concurrent access.
    ///
    /// Using this method bypasses critical WebSocket protocol management:
    /// - Automatic Ping/Pong frame handling stops working
    /// - Connection health monitoring is disabled
    /// - Protocol compliance becomes your responsibility
    ///
    /// Direct use of this API requires deep understanding of:
    /// - WebSocket protocol details including control frame handling
    /// - Proper bidirectional communication management
    /// - Connection health monitoring implementations
    /// - Thread safety and synchronization for concurrent access
    ///
    /// # Returns
    /// A tuple of the raw frame-handling stream and the read/write components.
    ///
    /// # Example
    /// ```no_run
    /// use yawc::WebSocket;
    ///
    /// async fn connect() -> yawc::Result<()> {
    ///     let ws = WebSocket::connect("ws://example.com/ws".parse()?).await?;
    ///     // Advanced usage - requires manual protocol handling
    ///     let (raw_stream, read_half, write_half) = unsafe { ws.split_stream() };
    ///     // Must implement control frame handling, connection monitoring etc.
    ///     Ok(())
    /// }
    /// ```
    pub unsafe fn split_stream(self) -> (Framed<HttpStream, Codec>, ReadHalf, WriteHalf) {
        let read_half = self.read_half;
        let write_half = self.write_half;
        let stream = self.stream;
        (stream, read_half, write_half)
    }

    /// Polls for the next frame in the WebSocket stream and handles protocol-level concerns.
    ///
    /// This method manages asynchronous frame retrieval and ensures proper protocol handling,
    /// including connection cleanup, frame validation, and error responses.
    ///
    /// # Protocol Flow
    /// 1. Polls underlying stream for next frame
    /// 2. For received frames:
    ///    - Processes and returns valid frames
    ///    - Sends appropriate close frame for protocol violations
    ///    - Flushes pending frames before connection closure
    /// 3. Begins cleanup when connection is closed
    ///
    /// # Frame Handling
    /// - Valid frames are processed and returned for application use
    /// - Protocol violations trigger graceful shutdown with appropriate status code
    /// - Control frames (ping/pong/close) receive special handling
    /// - Fragmented messages are reassembled before delivery
    ///
    /// # Error Responses
    /// - Size violations receive [`CloseCode::Size`]
    /// - Unsupported operations receive [`CloseCode::Unsupported`]
    /// - Protocol violations receive [`CloseCode::Protocol`]
    /// - Other errors receive [`CloseCode::Error`]
    ///
    /// # Parameters
    /// - `cx`: Task context for managing asynchronous polling
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(Frame))`: Successfully received and processed frame
    /// - `Poll::Ready(Err(WebSocketError))`: Protocol violation or other error
    /// - `Poll::Pending`: More data needed for complete frame
    pub fn poll_next_frame(&mut self, cx: &mut Context<'_>) -> Poll<Result<FrameView>> {
        let wake_proxy = Arc::clone(&self.wake_proxy);
        wake_proxy.set_waker(ContextKind::Read, cx.waker());

        loop {
            let res = wake_proxy.with_context(|cx| self.read_half.poll_frame(&mut self.stream, cx));
            match res {
                Poll::Ready(Err(WebSocketError::ConnectionClosed)) => {
                    ready!(wake_proxy.with_context(|cx| self.poll_flush_obligated(cx)))?;
                    return Poll::Ready(Err(WebSocketError::ConnectionClosed));
                }
                Poll::Ready(Ok(frame)) => match self.on_frame(frame)? {
                    Some(frame) => return Poll::Ready(Ok(frame)),
                    None => continue,
                },
                Poll::Ready(Err(err)) => {
                    let code = match err {
                        WebSocketError::FrameTooLarge => CloseCode::Size,
                        WebSocketError::InvalidOpCode(_) => CloseCode::Unsupported,
                        WebSocketError::ReservedBitsNotZero
                        | WebSocketError::ControlFrameFragmented
                        | WebSocketError::PingFrameTooLarge
                        | WebSocketError::InvalidFragment
                        | WebSocketError::InvalidContinuationFrame
                        | WebSocketError::CompressionNotSupported => CloseCode::Protocol,
                        _ => CloseCode::Error,
                    };
                    self.emit_close(FrameView::close(code, err.to_string()));
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

    /// Helper function to asynchronously retrieve the next frame from the WebSocket stream.
    ///
    /// This is a convenience wrapper around [`WebSocket::poll_next_frame`] that uses `poll_fn` to convert
    /// the polling interface into an async/await interface. It allows users to simply `await`
    /// the next frame rather than manually handling polling.
    ///
    /// # Returns
    /// - `Ok(Frame)` if a frame was successfully received
    /// - `Err(WebSocketError)` if an error occurred while receiving the frame
    pub async fn next_frame(&mut self) -> Result<FrameView> {
        poll_fn(|cx| self.poll_next_frame(cx)).await
    }

    /// Serializes data to JSON and sends it as a text frame over the WebSocket.
    ///
    /// This method takes a reference to data, serializes it to JSON, and sends it
    /// as a text frame over the WebSocket connection. The method is only available
    /// when the `json` feature is enabled.
    ///
    /// # Type Parameters
    /// - `T`: The type of data to serialize. Must implement `serde::Serialize`.
    ///
    /// # Parameters
    /// - `data`: Reference to the data to serialize and send.
    ///
    /// # Returns
    /// - `Ok(())` if the data was successfully serialized and sent.
    /// - `Err` if there was an error serializing the data or sending the frame.
    ///
    /// # Feature
    /// This method requires the `json` feature to be enabled in your Cargo.toml:
    /// ```toml
    /// [dependencies]
    /// yawc = { version = "0.1", features = ["json"] }
    /// ```
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub async fn send_json<T: serde::Serialize>(&mut self, data: &T) -> Result<()> {
        let bytes = serde_json::to_vec(data)?;
        futures::SinkExt::send(self, FrameView::text(bytes)).await
    }

    /// Creates a new client after an HTTP upgrade.
    fn new(role: Role, stream: HttpStream, opts: Negotitation) -> Self {
        let decoder = codec::Decoder::new(opts.max_payload_read);
        let encoder = codec::Encoder;
        let codec = Codec::from((decoder, encoder));

        Self {
            stream: Framed::new(stream, codec),
            read_half: ReadHalf::new(role, &opts),
            write_half: WriteHalf::new(role, &opts),
            wake_proxy: Arc::new(WakeProxy::default()),
            obligated_sends: VecDeque::new(),
            flush_sends: false,
            check_utf8: opts.utf8,
        }
    }

    fn on_frame(&mut self, frame: FrameView) -> Result<Option<FrameView>> {
        match frame.opcode {
            OpCode::Text => {
                if self.check_utf8 {
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
            OpCode::Binary | OpCode::Pong => Ok(Some(frame)),
            OpCode::Ping => {
                self.on_ping(frame);
                Ok(None)
            }
            OpCode::Close => match self.on_close(&frame) {
                Ok(_) => Ok(Some(frame)),
                Err(err) => Err(err),
            },
            OpCode::Continuation => unreachable!(),
        }
    }

    fn on_ping(&mut self, frame: FrameView) {
        self.obligated_sends
            .push_back(FrameView::pong(frame.payload));
    }

    fn on_close(&mut self, frame: &FrameView) -> Result<()> {
        match frame.payload.len() {
            0 => {}
            1 => return Err(WebSocketError::InvalidCloseFrame),
            _ => {
                let code = frame.close_code().expect("close code");

                if frame.close_reason().is_none() {
                    return Err(WebSocketError::InvalidUTF8);
                };

                if !code.is_allowed() {
                    self.emit_close(FrameView::close(CloseCode::Protocol, &frame.payload[2..]));
                    return Err(WebSocketError::InvalidCloseCode);
                }
            }
        }

        let frame = FrameView::close_raw(frame.payload.clone());
        self.emit_close(frame);

        Ok(())
    }

    fn emit_close(&mut self, frame: FrameView) {
        self.obligated_sends.push_back(frame);
        self.read_half.is_closed = true;
    }

    /// flush all the obligated payloads.
    fn poll_flush_obligated(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        while !self.obligated_sends.is_empty() {
            ready!(self.write_half.poll_ready(&mut self.stream, cx))?;

            let next = self.obligated_sends.pop_front().expect("obligated send");
            self.write_half.start_send(&mut self.stream, next)?;
            self.flush_sends = true;
        }

        if self.flush_sends {
            ready!(self.write_half.poll_flush(&mut self.stream, cx))?;
            self.flush_sends = true;
        }

        Poll::Ready(Ok(()))
    }
}

impl futures::Stream for WebSocket {
    type Item = FrameView;

    /// Polls for the next frame from the [`WebSocket`] stream.
    ///
    /// If the you want to receive the Result of reading from the [`WebSocket`], use [`WebSocket::poll_next_frame`] instead.
    ///
    /// This method handles asynchronous frame retrieval, managing both connection closure
    /// and frame-specific protocol compliance checks.
    ///
    /// The method will return `None` when the WebSocket connection has been closed, signaling the end
    /// of the stream. In most cases, [`WebSocket`] will complete any pending obligations and flush frames
    /// before fully terminating the connection.
    ///
    /// # Parameters
    /// - `cx`: The current task's execution context, used to manage asynchronous polling.
    ///
    /// # Returns
    /// - `Poll::Ready(Some(Frame))` if a frame is successfully received.
    /// - `Poll::Ready(None)` if the connection is closed, indicating no more frames will be received.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match ready!(this.poll_next_frame(cx)) {
            Ok(ok) => Poll::Ready(Some(ok)),
            Err(_) => Poll::Ready(None),
        }
    }
}

impl futures::Sink<FrameView> for WebSocket {
    type Error = WebSocketError;

    /// Checks if the [`WebSocket`] is ready to send a frame.
    ///
    /// This method first flushes any obligated frames, ensuring that pending data is sent
    /// before allowing new frames to be added. If the flush is successful, it polls the
    /// readiness of the `write_half` to send the next frame.
    ///
    /// # Parameters
    /// - `cx`: The current task's execution context, used to manage asynchronous polling.
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(()))` if the WebSocket is ready to send.
    /// - `Poll::Pending` if the WebSocket is not yet ready.
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

    /// Begins sending a frame to the WebSocket.
    ///
    /// Adds the specified frame to the `write_half` for transmission. This method does not
    /// immediately send the frame; it merely prepares the frame for sending.
    ///
    /// # Parameters
    /// - `item`: The `Frame` to be sent.
    ///
    /// # Returns
    /// - `Ok(())` if the frame was successfully prepared for sending.
    /// - `Err(WebSocketError)` if an error occurs while preparing the frame.
    fn start_send(self: Pin<&mut Self>, item: FrameView) -> std::result::Result<(), Self::Error> {
        let this = self.get_mut();
        this.write_half.start_send(&mut this.stream, item)
    }

    /// Polls to flush all pending frames in the WebSocket stream.
    ///
    /// Ensures that any frames waiting to be sent are fully flushed to the network.
    ///
    /// # Parameters
    /// - `cx`: The current task's execution context, used to manage asynchronous polling.
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(()))` if all pending frames have been flushed.
    /// - `Poll::Pending` if there are still frames waiting to be flushed.
    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        let this = self.get_mut();
        let wake_proxy = Arc::clone(&this.wake_proxy);
        wake_proxy.set_waker(ContextKind::Write, cx.waker());
        wake_proxy.with_context(|cx| this.write_half.poll_flush(&mut this.stream, cx))
    }

    /// Polls the connection to close the [`WebSocket`] stream.
    ///
    /// Initiates a graceful shutdown of the WebSocket by sending a close frame. This method
    /// will attempt to complete any pending frame transmissions before closing the connection.
    ///
    /// # Parameters
    /// - `cx`: The current task's execution context, used to manage asynchronous polling.
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(()))` if the WebSocket is successfully closed.
    /// - `Poll::Pending` if the close process is still in progress.
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

// ================ ReadHalf ====================

/// The read half of a WebSocket connection, responsible for receiving and processing incoming messages.
///
/// [`ReadHalf`] follows a sans-io design, meaning it does not handle I/O operations directly.
/// Instead, the user must provide a [`futures::Stream`](https://docs.rs/futures/latest/futures/stream/trait.Stream.html)
/// of frames on each function call. This allows the ReadHalf to be I/O agnostic and work with any underlying transport.
///
/// [`ReadHalf`] handles decompression and message fragmentation but does not manage WebSocket control frames.
/// Users are responsible for handling frames with [`OpCode::Ping`], [`OpCode::Pong`], and [`OpCode::Close`] codes.
///
/// After a [`OpCode::Close`] frame is received, the [`ReadHalf`] will no longer accept reads and will
/// return a [`WebSocketError::ConnectionClosed`] error for all subsequent read attempts.
///
/// # Warning
///
/// In most cases, you should **not** use [`ReadHalf`] directly. Instead, use
/// [`futures::StreamExt::split`](https://docs.rs/futures/latest/futures/stream/trait.StreamExt.html#method.split)
/// on the [`WebSocket`] to obtain a stream split that maintains all WebSocket protocol handling.
/// Direct use of [`ReadHalf`] bypasses important protocol management like automatic control frame handling.
///
///
/// # Example
/// ```no_run
/// use tokio::net::TcpStream;
/// use yawc::{WebSocket, frame::OpCode};
/// use futures::StreamExt;
/// use std::task::Context;
/// use std::pin::Pin;
/// use tokio_rustls::TlsConnector;
/// use url::Url;
///
/// #[tokio::main]
/// async fn main() -> yawc::Result<()> {
///     let url = "wss://api.example.com/ws".parse()?;
///     let ws = WebSocket::connect(url).await?;
///
///     let (mut stream, mut read_half, write_half) = unsafe { ws.split_stream() };
///
///     std::future::poll_fn(|cx| {
///         read_half.poll_frame(&mut stream, cx)
///     }).await?;
///
///     Ok(())
/// }
/// ```
pub struct ReadHalf {
    role: Role,
    /// Optional decompressor used to decompress incoming payloads.
    inflate: Option<Decompressor>,
    /// Structure for handling fragmented message frames.
    fragment: Option<Fragment>,
    /// Accumulated data from fragmented frames.
    accumulated: Vec<u8>,
    /// Maximum size of the read buffer
    ///
    /// When accumulating fragmented messages, once the read buffer exceeds this size
    /// it will be reset back to its initial capacity of 8 KiB. This helps prevent
    /// unbounded memory growth from receiving very large fragmented messages.
    max_read_buffer: usize,
    /// Indicates if the connection has been closed.
    is_closed: bool,
}

struct Fragment {
    opcode: OpCode,
    is_compressed: bool,
}

impl ReadHalf {
    fn new(role: Role, opts: &Negotitation) -> Self {
        let inflate = opts.decompressor(role);
        Self {
            role,
            inflate,
            max_read_buffer: opts.max_read_buffer,
            fragment: None,
            accumulated: Vec::with_capacity(1024),
            is_closed: false,
        }
    }

    /// Processes an incoming WebSocket frame, handling message fragmentation and decompression if required.
    ///
    /// This function manages both data frames (text and binary) and control frames (ping, pong, and close).
    /// For fragmented messages, it accumulates the fragments until the message is complete, handling
    /// decompression if needed. If an uncompressed frame is received but compression is required, an error
    /// is returned.
    ///
    /// - For `OpCode::Text` and `OpCode::Binary` frames:
    ///     - If the frame is final (`fin` flag set), it processes the frame as a complete message.
    ///     - If the frame is part of a fragmented message (`fin` flag not set), it starts accumulating
    ///       fragments or returns an error if a previous incomplete fragment exists.
    /// - For `OpCode::Continuation` frames:
    ///     - Continues accumulating data for the ongoing fragmented message.
    ///     - If the frame is final, the accumulated fragments are processed and assembled into a complete message.
    /// - For control frames:
    ///     - `Ping`, `Pong`, and `Close` frames are processed as-is if they are not fragmented.
    ///     - Control frames are returned to the user for optional custom handling.
    ///
    /// # Parameters
    /// - `frame`: The incoming frame to be processed.
    ///
    /// # Returns
    /// - `Ok(Some(Frame))` if the frame is ready for further processing.
    /// - `Ok(None)` if the frame is part of a fragmented message and not yet complete.
    /// - `Err(WebSocketError)` if an error occurs, such as invalid fragment or unsupported compression.
    fn on_frame(&mut self, mut frame: Frame) -> Result<Option<Frame>> {
        if frame.is_compressed && self.inflate.is_none() {
            return Err(WebSocketError::CompressionNotSupported);
        }

        if self.role == Role::Server {
            frame.unmask();
        }

        // log::debug!(
        //     "<<fin={} rsv1={} {:?}",
        //     frame.fin,
        //     frame.is_compressed,
        //     frame.opcode
        // );

        match frame.opcode {
            OpCode::Text | OpCode::Binary => {
                if self.fragment.is_some() {
                    return Err(WebSocketError::InvalidFragment);
                }

                if !frame.fin {
                    self.fragment = Some(Fragment {
                        opcode: frame.opcode,
                        is_compressed: frame.is_compressed,
                    });

                    self.accumulated.extend_from_slice(&frame.payload);

                    Ok(None)
                } else if frame.is_compressed {
                    let inflate = self.inflate.as_mut().unwrap();
                    let payload = inflate.decompress(&frame.payload, frame.fin)?;
                    if let Some(payload) = payload {
                        frame.is_compressed = false;
                        frame.payload = payload;
                        Ok(Some(frame))
                    } else {
                        Err(WebSocketError::InvalidFragment)
                    }
                } else {
                    Ok(Some(frame))
                }
            }
            OpCode::Continuation => {
                self.accumulated.extend_from_slice(&frame.payload);
                if self.accumulated.len() >= self.max_read_buffer {
                    return Err(WebSocketError::FrameTooLarge);
                }

                let fragment = self
                    .fragment
                    .as_ref()
                    .ok_or(WebSocketError::InvalidFragment)?;

                if frame.fin {
                    if fragment.is_compressed {
                        let inflate = self.inflate.as_mut().unwrap();
                        let output = inflate
                            .decompress(&self.accumulated, frame.fin)?
                            .expect("decompress output");

                        frame.opcode = fragment.opcode;
                        frame.is_compressed = false;
                        frame.payload = output;

                        // reset the buffer
                        unsafe { self.accumulated.set_len(0) };
                        // TODO: we need to improve this
                        self.accumulated.shrink_to(8192);
                        self.fragment = None;

                        Ok(Some(frame))
                    } else {
                        frame.opcode = fragment.opcode;
                        frame.payload.clear();
                        frame.payload.extend_from_slice(&self.accumulated);

                        // reset the buffer
                        unsafe { self.accumulated.set_len(0) };
                        // TODO: we need to improve this
                        self.accumulated.shrink_to(8192);
                        self.fragment = None;

                        Ok(Some(frame))
                    }
                } else {
                    Ok(None)
                }
            }
            _ => {
                if self.role == Role::Client && frame.is_masked() {
                    return Err(WebSocketError::InvalidFragment);
                }

                // Control frames cannot be fragmented
                if !frame.fin {
                    return Err(WebSocketError::InvalidFragment);
                }

                self.is_closed = frame.opcode == OpCode::Close;

                Ok(Some(frame))
            }
        }
    }

    /// Polls the WebSocket stream for the next frame, managing frame processing and protocol compliance.
    ///
    /// This method continuously polls the stream until one of three conditions is met:
    /// 1. A complete message frame is ready
    /// 2. An error occurs during frame processing
    /// 3. The WebSocket connection is closed
    ///
    /// # Message Types
    ///
    /// ## Data Frames
    /// Text and binary frames are processed according to the WebSocket protocol:
    /// - Complete frames are returned immediately
    /// - Fragmented frames are accumulated until the final fragment arrives
    /// - All fragments are validated for proper sequencing
    ///
    /// ## Control Frames
    /// Ping, Pong and Close frames receive special handling:
    /// - Always processed immediately when received
    /// - Never fragmented according to protocol
    /// - Close frames set internal closed state
    ///
    /// # Parameters
    /// - `stream`: The WebSocket stream to poll frames from
    /// - `cx`: Task context for asynchronous polling
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(Frame))`: A complete frame was successfully received
    /// - `Poll::Ready(Err(WebSocketError))`: An error occurred during frame processing
    /// - `Poll::Pending`: More data needed to complete current frame
    ///
    /// On connection closure (receiving Close frame or stream end),
    /// returns `Poll::Ready(Err(WebSocketError::ConnectionClosed))`.
    pub fn poll_frame<S>(&mut self, stream: &mut S, cx: &mut Context<'_>) -> Poll<Result<FrameView>>
    where
        S: futures::Stream<Item = Result<Frame>> + Unpin,
    {
        while !self.is_closed {
            let frame = ready!(stream.poll_next_unpin(cx));
            let res = match frame {
                Some(res) => res,
                None => {
                    self.is_closed = true;
                    break;
                }
            };

            match res {
                Ok(frame) => match self.on_frame(frame) {
                    Ok(Some(frame)) => {
                        return Poll::Ready(Ok(frame.into()));
                    }
                    Err(err) => return Poll::Ready(Err(err)),
                    Ok(None) => {}
                },
                Err(err) => return Poll::Ready(Err(err)),
            }
        }

        Poll::Ready(Err(WebSocketError::ConnectionClosed))
    }
}

// ================ WriteHalf ====================

/// Write half of the WebSocket connection.
///
/// [`WriteHalf`] manages sending WebSocket frames and closing the connection gracefully.
/// It handles:
///
/// - Compression of outgoing frames when enabled
/// - Masking of frames when acting as a client
/// - Protocol-compliant connection closure
/// - Frame buffering and flushing
///
/// # Warning
///
/// In most cases, you should **not** use [`WriteHalf`] directly. Instead, use
/// [`futures::StreamExt::split`](https://docs.rs/futures/latest/futures/stream/trait.StreamExt.html#method.split)
/// on the [`WebSocket`] to obtain stream halves that maintain all WebSocket protocol handling.
/// Direct use of [`WriteHalf`] bypasses important protocol management like automatic control frame handling.
///
/// # Connection Closure
/// When closing the connection, [`WriteHalf`] follows the WebSocket protocol by:
///
/// 1. Sending a [`OpCode::Close`] frame to the peer
/// 2. Flushing any pending frames
/// 3. Closing the underlying network stream
///
/// This ensures a clean shutdown where all data is delivered before disconnection.
///
/// # Example
/// ```no_run
/// use tokio::net::TcpStream;
/// use yawc::{WebSocket, frame::OpCode, Options, Result};
/// use futures::StreamExt;
/// use tokio_rustls::TlsConnector;
///
/// #[tokio::main]
/// async fn main() -> Result<()> {
///     // Connect WebSocket
///     let ws = WebSocket::connect("wss://example.com/ws".parse()?).await?;
///
///     // Split into read/write halves
///     let (stream, read_half, write_half) = unsafe { ws.split_stream() };
///     // do something ...
///     Ok(())
/// }
/// ```
pub struct WriteHalf {
    role: Role,
    buffer: BytesMut,
    deflate: Option<Compressor>,
    close_state: Option<CloseState>,
}

/// Represents the various states involved in gracefully closing a WebSocket connection.
///
/// The `CloseState` enum defines the sequential steps taken to close the WebSocket connection:
///
/// - `Sending(Frame)`: The WebSocket is in the process of sending an `OpCode::Close` frame to the peer.
/// - `Flushing`: The `Close` frame has been sent, and the connection is now flushing any remaining data.
/// - `Closing`: The WebSocket is closing the underlying stream, ensuring all resources are properly released.
/// - `Done`: The connection is fully closed, and no further actions are required.
enum CloseState {
    /// The WebSocket is sending the `Close` frame to signal the end of the connection.
    Sending(FrameView),
    /// The WebSocket is flushing the remaining data after the `Close` frame has been sent.
    Flushing,
    /// The WebSocket is in the process of closing the underlying network stream.
    Closing,
    /// The connection is fully closed, and all necessary shutdown steps have been completed.
    Done,
}

impl WriteHalf {
    fn new(role: Role, opts: &Negotitation) -> Self {
        let deflate = opts.compressor(role);
        Self {
            role,
            deflate,
            buffer: BytesMut::with_capacity(1024),
            close_state: None,
        }
    }

    /// Polls the readiness of the `WriteHalf` to send a new frame.
    ///
    /// If the WebSocket connection is already closed, this will return an error. Otherwise, it
    /// checks if the underlying stream is ready to accept a new frame.
    ///
    /// # Parameters
    /// - `stream`: The WebSocket stream to check readiness on
    /// - `cx`: The polling context
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(()))` if the `WriteHalf` is ready to send.
    /// - `Poll::Ready(Err(WebSocketError::ConnectionClosed))` if the connection is closed.
    pub fn poll_ready<S>(&mut self, stream: &mut S, cx: &mut Context<'_>) -> Poll<Result<()>>
    where
        S: futures::Sink<Frame, Error = WebSocketError> + Unpin,
    {
        if matches!(self.close_state, Some(CloseState::Done)) {
            return Poll::Ready(Err(WebSocketError::ConnectionClosed));
        }

        stream.poll_ready_unpin(cx)
    }

    /// Begins sending a frame through the `WriteHalf`.
    ///
    /// This method takes a FrameView representing the frame to be sent and prepares it for
    /// transmission according to the WebSocket protocol rules:
    ///
    /// - For non-control frames with compression enabled, the payload is compressed
    /// - For client connections, the frame is automatically masked per protocol requirements
    /// - Close frames trigger a state change to handle connection shutdown
    ///
    /// The method handles frame preparation but does not wait for the actual transmission to complete.
    /// The prepared frame is queued for sending in the underlying stream.
    ///
    /// # Parameters
    /// - `stream`: The WebSocket sink that will transmit the frame
    /// - `view`: A FrameView containing the frame payload and metadata
    ///
    /// # Returns
    /// - `Ok(())` if the frame was successfully prepared and queued
    /// - `Err(WebSocketError)` if frame preparation or queueing failed
    ///
    /// # Protocol Details
    /// - Non-control frames are compressed if compression is enabled
    /// - Client frames are masked per WebSocket protocol requirement
    /// - Close frames transition the connection to closing state
    pub fn start_send<S>(&mut self, stream: &mut S, view: FrameView) -> Result<()>
    where
        S: futures::Sink<Frame, Error = WebSocketError> + Unpin,
    {
        if view.opcode == OpCode::Close {
            self.close_state = Some(CloseState::Flushing);
        }

        let maybe_frame = if !view.opcode.is_control() {
            if let Some(deflate) = self.deflate.as_mut() {
                // Compress the payload if compression is enabled
                let output = deflate.compress(&view.payload)?;
                Some(Frame::compress(true, view.opcode, None, output))
            } else {
                None
            }
        } else {
            None
        };

        let mut frame = maybe_frame.unwrap_or_else(|| {
            self.buffer.extend_from_slice(&view.payload[..]);
            Frame::new(true, view.opcode, None, self.buffer.split())
        });

        if self.role == Role::Client {
            frame.mask();
        }

        stream.start_send_unpin(frame)
    }

    /// Polls to flush all pending frames in the `WriteHalf`.
    ///
    /// Ensures that any frames waiting to be sent are fully flushed to the network.
    ///
    /// # Parameters
    /// - `stream`: The WebSocket stream to flush frames on
    /// - `cx`: The polling context
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(()))` if all pending frames have been flushed.
    /// - `Poll::Pending` if there are still frames waiting to be flushed.
    pub fn poll_flush<S>(&mut self, stream: &mut S, cx: &mut Context<'_>) -> Poll<Result<()>>
    where
        S: futures::Sink<Frame, Error = WebSocketError> + Unpin,
    {
        stream.poll_flush_unpin(cx)
    }

    /// Polls the connection to close the [`WriteHalf`] by initiating a graceful shutdown.
    ///
    /// The `poll_close` function guides the closure process through several stages:
    /// 1. **Sending**: Sends a `Close` frame to the peer.
    /// 2. **Flushing**: Ensures the `Close` frame and any pending frames are fully flushed.
    /// 3. **Closing**: Closes the underlying network stream once the frames are flushed.
    /// 4. **Done**: Marks the connection as closed.
    ///
    /// This sequence ensures a clean shutdown, allowing all necessary data to be sent before the connection closes.
    ///
    /// # Parameters
    /// - `stream`: The WebSocket stream to close
    /// - `cx`: The polling context
    ///
    /// # Returns
    /// - `Poll::Ready(Ok(()))` once the connection is fully closed.
    /// - `Poll::Pending` if the closure is still in progress.
    pub fn poll_close<S>(&mut self, stream: &mut S, cx: &mut Context<'_>) -> Poll<Result<()>>
    where
        S: futures::Sink<Frame, Error = WebSocketError> + Unpin,
    {
        loop {
            match self.close_state.take() {
                None => {
                    let frame = FrameView::close(CloseCode::Normal, []);
                    self.close_state = Some(CloseState::Sending(frame));
                }
                Some(CloseState::Sending(frame)) => {
                    if stream.poll_ready_unpin(cx).is_pending() {
                        self.close_state = Some(CloseState::Sending(frame));
                        break Poll::Pending;
                    }

                    stream.start_send_unpin(frame.into())?;

                    self.close_state = Some(CloseState::Flushing);
                }
                Some(CloseState::Flushing) => {
                    if stream.poll_flush_unpin(cx).is_pending() {
                        self.close_state = Some(CloseState::Flushing);
                        break Poll::Pending;
                    }

                    self.close_state = Some(CloseState::Closing);
                }
                // TODO: we should probably wait for the read half close frame response
                Some(CloseState::Closing) => {
                    if stream.poll_close_unpin(cx).is_pending() {
                        self.close_state = Some(CloseState::Closing);
                        break Poll::Pending;
                    }

                    self.close_state = Some(CloseState::Done);
                }
                Some(CloseState::Done) => break Poll::Ready(Ok(())),
            }
        }
    }
}

#[cfg(feature = "reqwest")]
fn verify_reqwest(response: &reqwest::Response, options: Options) -> Result<Negotitation> {
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

    // max read buffer should be at least 2 times the payload read if not specified
    let max_read_buffer = options.max_read_buffer.unwrap_or(
        options
            .max_payload_read
            .map(|payload_read| payload_read * 2)
            .unwrap_or(MAX_READ_BUFFER),
    );

    Ok(Negotitation {
        extensions,
        compression_level,
        max_payload_read: options.max_payload_read.unwrap_or(MAX_PAYLOAD_READ),
        max_read_buffer,
        utf8: options.check_utf8,
    })
}

fn verify(response: &Response<Incoming>, options: Options) -> Result<Negotitation> {
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

    // max read buffer should be at least 2 times the payload read if not specified
    let max_read_buffer = options.max_read_buffer.unwrap_or(
        options
            .max_payload_read
            .map(|payload_read| payload_read * 2)
            .unwrap_or(MAX_READ_BUFFER),
    );

    Ok(Negotitation {
        extensions,
        compression_level,
        max_payload_read: options.max_payload_read.unwrap_or(MAX_PAYLOAD_READ),
        max_read_buffer,
        utf8: options.check_utf8,
    })
}

fn generate_key() -> String {
    use base64::prelude::*;
    let input: [u8; 16] = rand::random();
    BASE64_STANDARD.encode(input)
}

/// Creates a TLS connector with root certificates for secure WebSocket connections.
/// If the crypto provider hasn't been set, [*ring*](https://github.com/briansmith/ring) will be used.
///
/// Returns a TlsConnector configured with the system root certificates,
/// no client authentication, and HTTP/1.1 ALPN support.
///
/// # Implementation Details
/// - Uses webpki_roots for trusted root certificates
/// - Configures with the default crypto provider (or falls back to ring)
/// - Supports all TLS protocol versions
/// - Sets up HTTP/1.1 ALPN for protocol negotiation
///
/// # Panics
/// This function will panic if:
/// - The protocol versions specified in `rustls::ALL_VERSIONS` are not supported by the crypto provider
/// - The TLS configuration cannot be built with the given parameters
/// - The root certificate store cannot be properly initialized
fn tls_connector() -> TlsConnector {
    let mut root_cert_store = rustls::RootCertStore::empty();
    root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| TrustAnchor {
        subject: ta.subject.clone(),
        subject_public_key_info: ta.subject_public_key_info.clone(),
        name_constraints: ta.name_constraints.clone(),
    }));

    // define the provider if any, fallback to ring
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));

    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::ALL_VERSIONS)
        .expect("versions")
        .with_root_certificates(root_cert_store)
        .with_no_client_auth();
    config.alpn_protocols = vec!["http/1.1".into()];

    TlsConnector::from(Arc::new(config))
}
