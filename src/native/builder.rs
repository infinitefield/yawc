//! WebSocket connection builder.

use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{future::BoxFuture, FutureExt};
use tokio_rustls::TlsConnector;
use url::Url;

use super::{Options, Proxy, WebSocket};
use crate::{stream::MaybeTlsStream, Result};
use tokio::net::TcpStream;

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
/// The `S` parameter is the stream the finished connection runs on, and it defaults to
/// the plain or TLS socket that [`WebSocket::connect`] produces, so `WebSocketBuilder`
/// keeps meaning what it always did.
///
/// Only [`http_version`](Self::http_version) changes it, to
/// [`HttpStream`](super::HttpStream): an HTTP/2 WebSocket lives on one stream of a
/// multiplexed connection, so there is no socket to hand back. The builder methods are
/// shared, and only the `Future` impls differ.
pub struct WebSocketBuilder<S = MaybeTlsStream<TcpStream>> {
    pub(super) opts: Option<WsBuilderOpts>,
    pub(super) future: Option<BoxFuture<'static, Result<WebSocket<S>>>>,
}

/// Internal options structure for WebSocketBuilder.
///
/// Holds all the configuration options needed to establish a WebSocket connection,
/// including the target URL, TLS connector, connection options, and HTTP request builder.
pub(crate) struct WsBuilderOpts {
    pub(super) url: Url,
    pub(super) tcp_address: Option<SocketAddr>,
    pub(super) connector: Option<TlsConnector>,
    pub(super) proxy: Option<Proxy>,
    pub(super) establish_options: Option<Options>,
    pub(super) http_builder: Option<HttpRequestBuilder>,
    #[cfg(feature = "http2")]
    pub(super) version: HttpVersion,
}

impl<S> WebSocketBuilder<S> {
    /// Creates a new WebSocketBuilder with the specified URL.
    ///
    /// Initializes a builder with default settings that can be customized
    /// before establishing the connection.
    ///
    /// # Parameters
    /// - `url`: The WebSocket URL to connect to (ws:// or wss://)
    pub(super) fn new(url: Url) -> Self {
        Self {
            opts: Some(WsBuilderOpts {
                url,
                tcp_address: None,
                connector: None,
                proxy: None,
                establish_options: None,
                http_builder: None,
                #[cfg(feature = "http2")]
                version: HttpVersion::Http1,
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

    /// Dials through a SOCKS5 proxy instead of connecting to the host directly.
    ///
    /// The proxy is asked to open a tunnel to the URL's host, and everything above that
    /// runs through the tunnel unchanged, so a `wss://` connection still negotiates TLS
    /// end to end and the proxy sees only ciphertext.
    ///
    /// # Parameters
    /// - `proxy`: The proxy to dial through, from [`Proxy::socks5`]
    ///
    /// # Returns
    /// The builder for method chaining
    ///
    /// # Example
    /// ```no_run
    /// use yawc::{Proxy, WebSocket};
    ///
    /// async fn connect() -> yawc::Result<()> {
    ///     let ws = WebSocket::connect("wss://example.com/socket".parse()?)
    ///         .with_proxy(Proxy::socks5("socks5h://127.0.0.1:1080".parse()?)?)
    ///         .await?;
    ///
    ///     // Use WebSocket...
    ///     Ok(())
    /// }
    /// ```
    pub fn with_proxy(mut self, proxy: Proxy) -> Self {
        let Some(opts) = &mut self.opts else {
            unreachable!()
        };
        opts.proxy = Some(proxy);
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

#[cfg(feature = "http2")]
impl WebSocketBuilder {
    /// Selects the HTTP version used for the handshake.
    ///
    /// Reaching for this switches the connection to the RFC 8441 machinery and changes
    /// what the builder resolves to: [`HttpWebSocket`](super::HttpWebSocket) instead of
    /// [`TcpWebSocket`](super::TcpWebSocket). An HTTP/2 WebSocket lives on one stream of
    /// a multiplexed connection, so there is no underlying socket to hand back.
    ///
    /// Leaving this alone keeps the HTTP/1.1 handshake and the existing return type,
    /// which is what almost every peer wants: RFC 8441 support is rare enough that
    /// HTTP/2 is worth asking for only when the server is known to implement it.
    ///
    /// Every other builder method is available before or after this call.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use yawc::{HttpVersion, WebSocket};
    ///
    /// # async fn example() -> yawc::Result<()> {
    /// let ws = WebSocket::connect("wss://example.com/chat".parse()?)
    ///     .http_version(HttpVersion::Http2)
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    #[cfg_attr(docsrs, doc(cfg(feature = "http2")))]
    pub fn http_version(mut self, version: HttpVersion) -> WebSocketBuilder<super::HttpStream> {
        let Some(mut opts) = self.opts.take() else {
            unreachable!()
        };
        opts.version = version;

        WebSocketBuilder {
            opts: Some(opts),
            future: None,
        }
    }
}

/// The HTTP version used to carry a WebSocket connection.
///
/// HTTP/1.1 uses the RFC 6455 `Upgrade` handshake. HTTP/2 uses the RFC 8441 extended
/// CONNECT handshake, which puts the connection on a single stream of a multiplexed
/// HTTP/2 connection.
#[cfg(feature = "http2")]
#[cfg_attr(docsrs, doc(cfg(feature = "http2")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HttpVersion {
    /// Always use the HTTP/1.1 `Upgrade` handshake.
    #[default]
    Http1,

    /// Use the HTTP/2 extended CONNECT handshake.
    ///
    /// Over `wss://` this offers only the `h2` ALPN protocol, so a server that cannot
    /// speak HTTP/2 fails the TLS handshake rather than silently falling back. Over
    /// `ws://` it assumes HTTP/2 prior knowledge, which only works against a server
    /// configured to expect it.
    ///
    /// This is an explicit choice because RFC 8441 support is rare: negotiating `h2` only
    /// means the peer speaks HTTP/2, and most deployments serve `h2` for ordinary
    /// requests while accepting WebSockets over HTTP/1.1 only. Against such a peer this
    /// fails rather than downgrading, so use it when the server is known to implement
    /// RFC 8441. Everything else should stay on the default HTTP/1.1 handshake.
    ///
    /// # Trying HTTP/2 first
    ///
    /// There is no built-in negotiation, because ALPN cannot answer whether the peer
    /// implements RFC 8441 and guessing wrong costs a wasted connection. A caller who
    /// wants to try anyway can ask for it and fall back:
    ///
    /// ```no_run
    /// use yawc::{HttpVersion, WebSocket};
    ///
    /// # async fn example(url: url::Url) -> yawc::Result<()> {
    /// let ws = match WebSocket::connect(url.clone())
    ///     .http_version(HttpVersion::Http2)
    ///     .await
    /// {
    ///     Ok(ws) => ws,
    ///     // The peer answered, but not with a WebSocket. Nothing on this connection is
    ///     // salvageable, and ALPN has already committed it to HTTP/2, so the retry has
    ///     // to dial again.
    ///     Err(err) if err.is_handshake_error() => {
    ///         WebSocket::connect(url)
    ///             .http_version(HttpVersion::Http1)
    ///             .await?
    ///     }
    ///     Err(err) => return Err(err),
    /// };
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Written out this way the cost is visible: against a peer without RFC 8441, which
    /// is most of them, every connection pays a full TCP and TLS handshake before the
    /// one that works. Worth it when the peer is genuinely unknown, not when it is not.
    Http2,
}

#[cfg(feature = "http2")]
impl Future for WebSocketBuilder<super::HttpStream> {
    type Output = Result<super::HttpWebSocket>;

    /// Polls the future to establish the WebSocket connection.
    ///
    /// Mirrors the default builder's poll, but resolves to an
    /// [`HttpWebSocket`](super::HttpWebSocket): the HTTP/2 handshake hands back a stream
    /// of a multiplexed connection rather than the socket underneath it.
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(opts) = this.opts.take() {
            let future = super::connect_versioned(opts);
            this.future = Some(Box::pin(future));
        }

        let Some(pinned) = &mut this.future else {
            unreachable!()
        };
        pinned.poll_unpin(cx)
    }
}

impl Future for WebSocketBuilder {
    type Output = Result<WebSocket<MaybeTlsStream<TcpStream>>>;

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
            let future = WebSocket::connect_priv(opts);
            this.future = Some(Box::pin(future));
        }

        let Some(pinned) = &mut this.future else {
            unreachable!()
        };
        pinned.poll_unpin(cx)
    }
}
