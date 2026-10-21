use std::{
    pin::{pin, Pin},
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
#[cfg(feature = "tls")]
use tokio_rustls::client::TlsStream;

/// A stream that might be protected with TLS.
#[non_exhaustive]
#[derive(Debug)]
#[allow(clippy::module_name_repetitions)]
#[allow(clippy::large_enum_variant)]
pub enum MaybeTlsStream<S> {
    /// Unencrypted socket stream.
    Plain(S),
    /// Encrypted socket stream using `rustls`.
    #[cfg(feature = "tls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
    Tls(TlsStream<S>),
}

impl<S> From<S> for MaybeTlsStream<S> {
    fn from(value: S) -> Self {
        Self::Plain(value)
    }
}

#[cfg(feature = "tls")]
impl<S> From<TlsStream<S>> for MaybeTlsStream<S> {
    fn from(value: TlsStream<S>) -> Self {
        Self::Tls(value)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for MaybeTlsStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => pin!(s).poll_read(cx, buf),
            #[cfg(feature = "tls")]
            MaybeTlsStream::Tls(s) => pin!(s).poll_read(cx, buf),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for MaybeTlsStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => pin!(s).poll_write(cx, buf),
            #[cfg(feature = "tls")]
            MaybeTlsStream::Tls(s) => pin!(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => pin!(s).poll_flush(cx),
            #[cfg(feature = "tls")]
            MaybeTlsStream::Tls(s) => pin!(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => pin!(s).poll_shutdown(cx),
            #[cfg(feature = "tls")]
            MaybeTlsStream::Tls(s) => pin!(s).poll_shutdown(cx),
        }
    }
}
