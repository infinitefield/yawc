#![cfg(not(target_arch = "wasm32"))]

#[cfg(not(feature = "tls"))]
mod without_tls {
    use futures::FutureExt;
    use yawc::{Proxy, WebSocket, WebSocketError};

    #[tokio::test]
    async fn rejects_wss_before_dialing() {
        let result =
            WebSocket::connect("wss://example.invalid/socket".parse().unwrap()).now_or_never();

        assert!(matches!(result, Some(Err(WebSocketError::TlsDisabled))));
    }

    #[tokio::test]
    async fn rejects_wss_before_contacting_a_proxy() {
        let proxy = Proxy::socks5("socks5h://proxy.invalid".parse().unwrap()).unwrap();
        let result = WebSocket::connect("wss://example.invalid/socket".parse().unwrap())
            .with_proxy(proxy)
            .now_or_never();

        assert!(matches!(result, Some(Err(WebSocketError::TlsDisabled))));
    }

    #[cfg(feature = "http2")]
    #[tokio::test]
    async fn rejects_wss_for_both_http_versions() {
        use yawc::HttpVersion;

        for version in [HttpVersion::Http1, HttpVersion::Http2] {
            let result = WebSocket::connect("wss://example.invalid/socket".parse().unwrap())
                .http_version(version)
                .now_or_never();

            assert!(matches!(result, Some(Err(WebSocketError::TlsDisabled))));
        }
    }
}

#[cfg(all(
    feature = "tls",
    not(any(feature = "rustls-ring", feature = "rustls-aws-lc-rs"))
))]
#[tokio::test]
async fn requires_a_custom_connector_before_dialing() {
    use futures::FutureExt;
    use yawc::{WebSocket, WebSocketError};

    let result = WebSocket::connect("wss://example.invalid/socket".parse().unwrap()).now_or_never();

    assert!(matches!(
        result,
        Some(Err(WebSocketError::MissingTlsConnector))
    ));
}

#[cfg(feature = "tls")]
mod with_tls {
    use std::{future::Future, io::Cursor, sync::Arc, time::Duration};

    use rustls::{server::Acceptor, ClientConfig, RootCertStore};
    use tokio::{io::AsyncReadExt, net::TcpListener, time::timeout};
    use tokio_rustls::TlsConnector;
    use yawc::{WebSocket, WebSocketError};

    async fn read_alpn(listener: TcpListener) -> Vec<Vec<u8>> {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut acceptor = Acceptor::default();
        let mut buf = [0; 4096];

        loop {
            let len = stream.read(&mut buf).await.unwrap();
            assert_ne!(len, 0, "client closed before sending a TLS ClientHello");
            acceptor.read_tls(&mut Cursor::new(&buf[..len])).unwrap();
            if let Some(accepted) = acceptor.accept().unwrap() {
                return accepted
                    .client_hello()
                    .alpn()
                    .unwrap()
                    .map(Vec::from)
                    .collect();
            }
        }
    }

    async fn offered_alpn<S>(
        client: impl Future<Output = yawc::Result<S>>,
        listener: TcpListener,
    ) -> Vec<Vec<u8>> {
        let (result, alpn) = timeout(Duration::from_secs(5), async {
            tokio::join!(client, read_alpn(listener))
        })
        .await
        .expect("TLS ClientHello should complete without a WebSocket handshake");

        // The mock peer closes after inspecting ClientHello, without sending a certificate.
        assert!(matches!(result, Err(WebSocketError::IoError(_))));
        alpn
    }

    #[tokio::test]
    async fn uses_the_custom_connector() {
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(RootCertStore::empty())
                .with_no_client_auth();
        config.alpn_protocols = vec![b"custom-protocol".to_vec()];

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = WebSocket::connect("wss://example.com/socket".parse().unwrap())
            .with_tcp_address(listener.local_addr().unwrap())
            .with_connector(TlsConnector::from(Arc::new(config)));

        assert_eq!(
            offered_alpn(client, listener).await,
            [b"custom-protocol".to_vec()]
        );
    }

    #[cfg(any(feature = "rustls-ring", feature = "rustls-aws-lc-rs"))]
    #[tokio::test]
    async fn uses_the_fallback_connector() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = WebSocket::connect("wss://example.com/socket".parse().unwrap())
            .with_tcp_address(listener.local_addr().unwrap());

        assert_eq!(offered_alpn(client, listener).await, [b"http/1.1".to_vec()]);
    }

    #[cfg(all(
        feature = "http2",
        any(feature = "rustls-ring", feature = "rustls-aws-lc-rs")
    ))]
    #[tokio::test]
    async fn offers_h2_with_the_fallback_connector() {
        use yawc::HttpVersion;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = WebSocket::connect("wss://example.com/socket".parse().unwrap())
            .with_tcp_address(listener.local_addr().unwrap())
            .http_version(HttpVersion::Http2);

        assert_eq!(offered_alpn(client, listener).await, [b"h2".to_vec()]);
    }
}
