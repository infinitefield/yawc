//! Native WebSocket implementation for Tokio runtime.

mod builder;
mod options;
mod split;
mod upgrade;

use crate::{close, codec, compression, frame, stream, Result, WebSocketError};

use {
    bytes::Bytes,
    http_body_util::Empty,
    hyper::{body::Incoming, header, upgrade::Upgraded, Request, Response, StatusCode},
    hyper_util::rt::TokioIo,
    stream::MaybeTlsStream,
    tokio::net::TcpStream,
    tokio_rustls::{rustls::pki_types::ServerName, TlsConnector},
};

use std::{
    collections::VecDeque,
    future::poll_fn,
    io,
    net::SocketAddr,
    pin::{pin, Pin},
    str::FromStr,
    sync::Arc,
    task::{ready, Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite};

use close::CloseCode;
use codec::Codec;
use compression::{Compressor, Decompressor, WebSocketExtensions};
use futures::task::AtomicWaker;
use tokio_rustls::rustls::{self, pki_types::TrustAnchor};
use tokio_util::codec::Framed;
use url::Url;

// Re-exports
pub use builder::{HttpRequest, HttpRequestBuilder, WebSocketBuilder};
pub use frame::{Frame, OpCode};
pub use options::{CompressionLevel, DeflateOptions, Options};
pub use split::{ReadHalf, WriteHalf};
pub use upgrade::UpgradeFut;

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
}

impl Negotiation {
    pub(crate) fn decompressor(&self, role: Role) -> Option<Decompressor> {
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
                Decompressor::no_context_takeover()
            } else {
                #[cfg(feature = "zlib")]
                if let Some(window_bits) = config.client_max_window_bits {
                    Decompressor::new_with_window_bits(window_bits)
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
                if let Some(window_bits) = config.server_max_window_bits {
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
                Compressor::no_context_takeover(level)
            } else {
                #[cfg(feature = "zlib")]
                if let Some(window_bits) = config.client_max_window_bits {
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
                if let Some(window_bits) = config.server_max_window_bits {
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
pub struct WebSocket {
    stream: Framed<HttpStream, Codec>,
    read_half: ReadHalf,
    write_half: WriteHalf,
    wake_proxy: Arc<WakeProxy>,
    obligated_sends: VecDeque<Frame>,
    flush_sends: bool,
    check_utf8: bool,
}

impl WebSocket {
    // ================== Client ====================

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
    ) -> Result<WebSocket> {
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

        Self::handshake_with_request(url, stream, options, builder).await
    }

    /// Performs a WebSocket handshake over an existing connection.
    pub async fn handshake<S>(url: Url, io: S, options: Options) -> Result<WebSocket>
    where
        S: AsyncWrite + AsyncRead + Send + Unpin + 'static,
    {
        Self::handshake_with_request(url, io, options, HttpRequest::builder()).await
    }

    /// Performs a WebSocket handshake with a customizable HTTP request.
    pub async fn handshake_with_request<S>(
        url: Url,
        io: S,
        options: Options,
        mut builder: HttpRequestBuilder,
    ) -> Result<WebSocket>
    where
        S: AsyncWrite + AsyncRead + Send + Unpin + 'static,
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
    #[cfg(feature = "reqwest")]
    #[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
    pub async fn reqwest(
        mut url: Url,
        client: reqwest::Client,
        options: Options,
    ) -> Result<WebSocket> {
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
            negotiated,
        ))
    }

    // ================== Server ====================

    /// Upgrades an HTTP connection to a WebSocket one.
    pub fn upgrade<B>(request: impl std::borrow::BorrowMut<Request<B>>) -> UpgradeResult {
        Self::upgrade_with_options(request, Options::default())
    }

    /// Attempts to upgrade an incoming `hyper::Request` to a WebSocket connection with customizable options.
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
            }),
        };

        Ok((response, stream))
    }

    // ======== common websocket functions =============

    /// Splits the [`WebSocket`] into its low-level components for advanced usage.
    ///
    /// # Safety
    /// This function is unsafe because it splits ownership of shared state.
    pub unsafe fn split_stream(self) -> (Framed<HttpStream, Codec>, ReadHalf, WriteHalf) {
        (self.stream, self.read_half, self.write_half)
    }

    /// Polls for the next frame in the WebSocket stream.
    pub fn poll_next_frame(&mut self, cx: &mut Context<'_>) -> Poll<Result<Frame>> {
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

    /// Serializes data to JSON and sends it as a text frame.
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub async fn send_json<T: serde::Serialize>(&mut self, data: &T) -> Result<()> {
        let bytes = serde_json::to_vec(data)?;
        futures::SinkExt::send(self, Frame::text(bytes)).await
    }

    /// Sends a message as multiple fragmented frames.
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

    /// Creates a new connection after an HTTP upgrade.
    pub(crate) fn new(role: Role, stream: HttpStream, opts: Negotiation) -> Self {
        let decoder = codec::Decoder::new(role, opts.max_payload_read);
        let encoder = codec::Encoder::new(role);
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

    fn on_frame(&mut self, frame: Frame) -> Result<Option<Frame>> {
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

    fn on_ping(&mut self, frame: Frame) {
        self.obligated_sends.push_back(Frame::pong(frame.payload));
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

impl futures::Stream for WebSocket {
    type Item = Frame;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match ready!(this.poll_next_frame(cx)) {
            Ok(ok) => Poll::Ready(Some(ok)),
            Err(_) => Poll::Ready(None),
        }
    }
}

impl futures::Sink<Frame> for WebSocket {
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
        this.write_half.start_send(&mut this.stream, item)
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
