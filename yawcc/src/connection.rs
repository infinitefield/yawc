use std::{fs::File, io::BufReader, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{bail, ensure, Context};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::Args as ClapArgs;
use http_body_util::Empty;
use hyper::{
    body::Bytes,
    header::{self, HeaderMap, HeaderName, HeaderValue},
    Request, StatusCode,
};
use hyper_util::rt::TokioIo;
use percent_encoding::percent_decode_str;
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{lookup_host, TcpStream},
    time::timeout,
};
use tokio_rustls::{
    rustls::{
        self,
        pki_types::{CertificateDer, ServerName, UnixTime},
        ClientConfig, RootCertStore,
    },
    TlsConnector,
};
use url::Url;
use yawc::{Options, Role, WebSocket};

trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}
type Stream = Box<dyn Io>;
pub type Socket = WebSocket<TokioIo<hyper::upgrade::Upgraded>>;

#[derive(ClapArgs)]
pub struct Args {
    /// WebSocket URL (ws:// or wss://).
    pub url: Url,
    /// Total deadline for DNS, proxy, TLS, handshake, and redirects.
    #[arg(short = 't', long, default_value = "5s", value_parser = humantime::parse_duration)]
    timeout: Duration,
    /// Custom HTTP header in Key: Value format; repeat for multiple headers.
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,
    /// Override the TCP destination while retaining URL Host and TLS server name.
    #[arg(long)]
    tcp_host: Option<String>,
    /// Proxy URL: socks5:// (local DNS), socks5h:// (proxy DNS), or http[s]:// (CONNECT).
    #[arg(long)]
    proxy: Option<Url>,
    /// Offered WebSocket subprotocol; repeat in preference order.
    #[arg(short = 's', long = "subprotocol")]
    subprotocols: Vec<String>,
    /// Origin header.
    #[arg(short = 'o', long)]
    origin: Option<String>,
    /// Basic HTTP authentication as username:password.
    #[arg(long)]
    auth: Option<String>,
    /// Host header override (does not change TLS server name).
    #[arg(long)]
    host: Option<String>,
    /// Additional trusted CA certificates in PEM format.
    #[arg(long)]
    ca: Option<PathBuf>,
    /// Client certificate chain in PEM format; requires --key.
    #[arg(long, requires = "key")]
    cert: Option<PathBuf>,
    /// Unencrypted client private key in PEM format; requires --cert.
    #[arg(long, requires = "cert")]
    key: Option<PathBuf>,
    /// Disable server certificate verification.
    #[arg(short = 'n', long = "no-check")]
    no_check: bool,
    /// Follow HTTP redirects. HTTPS/WSS downgrades are rejected.
    #[arg(short = 'L', long = "location")]
    location: bool,
    /// Maximum redirects with --location.
    #[arg(long, default_value_t = 10)]
    max_redirects: usize,
}

pub struct Config {
    args: Args,
    headers: HeaderMap,
    tls: TlsConnector,
    proxy_tls: TlsConnector,
}

impl Config {
    pub fn new(mut args: Args) -> anyhow::Result<Self> {
        validate_url(&args.url)?;
        ensure!(
            std::time::Instant::now()
                .checked_add(args.timeout)
                .is_some(),
            "timeout is too large"
        );
        if let Some(proxy) = &args.proxy {
            ensure!(
                matches!(proxy.scheme(), "http" | "https" | "socks5" | "socks5h"),
                "unsupported proxy scheme"
            );
            ensure!(proxy.host().is_some(), "proxy must have a host");
            ensure!(
                proxy.path().is_empty() || proxy.path() == "/",
                "proxy URL must not contain a path"
            );
            ensure!(
                proxy.query().is_none() && proxy.fragment().is_none(),
                "proxy URL must not contain a query or fragment"
            );
        }
        let mut headers = HeaderMap::new();
        for raw in &args.headers {
            let (key, value) = raw
                .split_once(':')
                .context("header must have Key: Value format")?;
            let key =
                HeaderName::from_bytes(key.trim().as_bytes()).context("invalid header name")?;
            ensure!(
                !key.as_str().starts_with("sec-websocket-")
                    && key != header::CONNECTION
                    && key != header::UPGRADE
                    && key != header::PROXY_AUTHORIZATION,
                "header {key} is managed by yawcc; use its dedicated options"
            );
            let value =
                HeaderValue::from_str(value.trim_start()).context("invalid header value")?;
            headers.append(key, value);
        }
        for (key, value) in [(header::ORIGIN, &args.origin), (header::HOST, &args.host)] {
            if let Some(value) = value {
                headers.insert(
                    key,
                    HeaderValue::from_str(value).context("invalid origin or host")?,
                );
            }
        }
        if let Some(auth) = &args.auth {
            ensure!(
                auth.contains(':'),
                "--auth must have username:password format"
            );
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {}", STANDARD.encode(auth)))?,
            );
        }
        if !args.url.username().is_empty() || args.url.password().is_some() {
            ensure!(
                args.auth.is_none() && !headers.contains_key(header::AUTHORIZATION),
                "specify authentication either in the URL or headers/--auth"
            );
            let auth = format!(
                "{}:{}",
                decode(args.url.username())?,
                decode(args.url.password().unwrap_or(""))?
            );
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {}", STANDARD.encode(auth)))?,
            );
            let _ = args.url.set_username("");
            let _ = args.url.set_password(None);
        }
        for (index, protocol) in args.subprotocols.iter().enumerate() {
            ensure!(
                !protocol.is_empty() && protocol.bytes().all(is_token),
                "invalid WebSocket subprotocol"
            );
            ensure!(
                !args.subprotocols[..index].contains(protocol),
                "duplicate WebSocket subprotocol"
            );
        }
        let tls = tls_connector(&args, true)?;
        let proxy_tls = tls_connector(&args, false)?;
        Ok(Self {
            args,
            headers,
            tls,
            proxy_tls,
        })
    }

    pub async fn connect(&self) -> anyhow::Result<Socket> {
        timeout(self.args.timeout, self.connect_inner())
            .await
            .context("connection deadline exceeded")?
    }

    async fn connect_inner(&self) -> anyhow::Result<Socket> {
        let mut url = self.args.url.clone();
        let mut headers = self.headers.clone();
        let mut use_override = true;
        for hop in 0..=self.args.max_redirects {
            let io = self.dial(&url, use_override).await?;
            let key = STANDARD.encode(rand::random::<[u8; 16]>());
            let mut request = Request::builder()
                .method("GET")
                .uri(&url[url::Position::BeforePath..url::Position::AfterQuery]);
            *request.headers_mut().context("invalid request")? = headers.clone();
            let h = request.headers_mut().context("invalid request")?;
            if !h.contains_key(header::HOST) {
                h.insert(
                    header::HOST,
                    HeaderValue::from_str(&authority(&url, false))?,
                );
            }
            h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
            h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
            h.insert(
                header::SEC_WEBSOCKET_VERSION,
                HeaderValue::from_static("13"),
            );
            h.insert(header::SEC_WEBSOCKET_KEY, HeaderValue::from_str(&key)?);
            h.insert(
                header::SEC_WEBSOCKET_EXTENSIONS,
                HeaderValue::from_static("permessage-deflate"),
            );
            if !self.args.subprotocols.is_empty() {
                h.insert(
                    header::SEC_WEBSOCKET_PROTOCOL,
                    HeaderValue::from_str(&self.args.subprotocols.join(", "))?,
                );
            }
            let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
                .max_buf_size(32 * 1024)
                .handshake(TokioIo::new(io))
                .await?;
            tokio::spawn(async move {
                let _ = conn.with_upgrades().await;
            });
            let mut response = sender
                .send_request(request.body(Empty::<Bytes>::new())?)
                .await
                .context("WebSocket handshake request failed")?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308)
                && self.args.location
            {
                ensure!(hop < self.args.max_redirects, "maximum redirects exceeded");
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .context("redirect has no Location header")?
                    .to_str()?;
                let mut next = url.join(location).context("invalid redirect URL")?;
                match next.scheme() {
                    "http" => {
                        let _ = next.set_scheme("ws");
                    }
                    "https" => {
                        let _ = next.set_scheme("wss");
                    }
                    _ => {}
                }
                validate_url(&next)?;
                ensure!(
                    next.username().is_empty() && next.password().is_none(),
                    "redirect URL must not contain credentials"
                );
                ensure!(
                    url.scheme() != "wss" || next.scheme() == "wss",
                    "refusing redirect from WSS to WS"
                );
                if origin(&url) != origin(&next) {
                    ensure!(
                        self.args.cert.is_none(),
                        "refusing cross-origin redirect with a client certificate"
                    );
                    // Custom headers may contain credentials under arbitrary names.
                    headers.clear();
                    use_override = false;
                }
                url = next;
                continue;
            }
            ensure!(
                response.status() == StatusCode::SWITCHING_PROTOCOLS,
                "WebSocket handshake returned HTTP {}",
                response.status()
            );
            let h = response.headers();
            ensure!(
                has_token(h, header::CONNECTION, "upgrade")
                    && has_token(h, header::UPGRADE, "websocket"),
                "invalid WebSocket upgrade response"
            );
            let expected = STANDARD.encode(Sha1::digest(format!(
                "{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
            )));
            ensure!(
                h.get_all(header::SEC_WEBSOCKET_ACCEPT).iter().count() == 1
                    && h.get(header::SEC_WEBSOCKET_ACCEPT)
                        .is_some_and(|v| v.as_bytes() == expected.as_bytes()),
                "invalid Sec-WebSocket-Accept response"
            );
            let protocols: Vec<_> = h.get_all(header::SEC_WEBSOCKET_PROTOCOL).iter().collect();
            ensure!(
                protocols.len() <= 1,
                "server returned multiple subprotocols"
            );
            match protocols.first() {
                Some(value) => {
                    let selected = value.to_str()?;
                    ensure!(
                        self.args.subprotocols.iter().any(|p| p == selected),
                        "server selected an unoffered subprotocol"
                    );
                    eprintln!("Subprotocol: {selected}");
                }
                None => ensure!(
                    self.args.subprotocols.is_empty(),
                    "server did not select an offered subprotocol"
                ),
            }
            let extension_values = h
                .get_all(header::SEC_WEBSOCKET_EXTENSIONS)
                .iter()
                .map(|v| v.to_str())
                .collect::<Result<Vec<_>, _>>()?;
            let extensions = parse_extensions(&extension_values)?;
            let upgraded = hyper::upgrade::on(&mut response).await?;
            return WebSocket::from_stream_with_extensions(
                TokioIo::new(upgraded),
                Role::Client,
                extensions.as_deref(),
                Options::default().with_utf8().with_balanced_compression(),
            )
            .map_err(Into::into);
        }
        bail!("maximum redirects exceeded")
    }

    async fn dial(&self, url: &Url, use_override: bool) -> anyhow::Result<Stream> {
        let host = host_name(url)?;
        let port = url.port_or_known_default().context("URL has no port")?;
        let override_address = if use_override {
            self.args.tcp_host.as_deref()
        } else {
            None
        };
        let mut io: Stream = if let Some(proxy) = &self.args.proxy {
            if matches!(proxy.scheme(), "socks5" | "socks5h") {
                let address = match override_address {
                    Some(address) => Some(first_address(address).await?),
                    None => None,
                };
                let tcp = yawc::Proxy::socks5(proxy.clone())?
                    .connect(url, address)
                    .await
                    .context("SOCKS5 tunnel failed")?;
                tcp.set_nodelay(true)?;
                Box::new(tcp)
            } else {
                let proxy_host = host_name(proxy)?;
                let proxy_port = proxy.port_or_known_default().context("proxy has no port")?;
                let tcp = TcpStream::connect((proxy_host.as_str(), proxy_port))
                    .await
                    .context("connect to proxy")?;
                tcp.set_nodelay(true)?;
                let proxy_io: Stream = if proxy.scheme() == "https" {
                    Box::new(
                        self.proxy_tls
                            .connect(ServerName::try_from(proxy_host)?, tcp)
                            .await
                            .context("proxy TLS handshake failed")?,
                    )
                } else {
                    Box::new(tcp)
                };
                let destination = if let Some(address) = override_address {
                    first_address(address).await?.to_string()
                } else {
                    authority(url, true)
                };
                let mut request = Request::builder()
                    .method("CONNECT")
                    .uri(&destination)
                    .header(header::HOST, &destination);
                if !proxy.username().is_empty() || proxy.password().is_some() {
                    let auth = format!(
                        "{}:{}",
                        decode(proxy.username())?,
                        decode(proxy.password().unwrap_or(""))?
                    );
                    request = request.header(
                        header::PROXY_AUTHORIZATION,
                        format!("Basic {}", STANDARD.encode(auth)),
                    );
                }
                let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
                    .max_buf_size(32 * 1024)
                    .handshake(TokioIo::new(proxy_io))
                    .await?;
                tokio::spawn(async move {
                    let _ = conn.with_upgrades().await;
                });
                let mut response = sender
                    .send_request(request.body(Empty::<Bytes>::new())?)
                    .await
                    .context("proxy CONNECT failed")?;
                ensure!(
                    response.status().is_success(),
                    "proxy CONNECT returned HTTP {}",
                    response.status()
                );
                Box::new(TokioIo::new(hyper::upgrade::on(&mut response).await?))
            }
        } else {
            let tcp = if let Some(address) = override_address {
                TcpStream::connect(address).await
            } else {
                TcpStream::connect((host.as_str(), port)).await
            }
            .context("TCP connection failed")?;
            tcp.set_nodelay(true)?;
            Box::new(tcp)
        };
        if url.scheme() == "wss" {
            io = Box::new(
                self.tls
                    .connect(ServerName::try_from(host)?, io)
                    .await
                    .context("TLS handshake failed")?,
            );
        }
        Ok(io)
    }
}

fn validate_url(url: &Url) -> anyhow::Result<()> {
    ensure!(
        matches!(url.scheme(), "ws" | "wss"),
        "URL must use ws:// or wss://"
    );
    ensure!(url.host().is_some(), "URL must have a host");
    ensure!(
        url.fragment().is_none(),
        "WebSocket URL must not contain a fragment"
    );
    Ok(())
}
fn host_name(url: &Url) -> anyhow::Result<String> {
    Ok(match url.host().context("URL has no host")? {
        url::Host::Domain(h) => h.to_owned(),
        url::Host::Ipv4(h) => h.to_string(),
        url::Host::Ipv6(h) => h.to_string(),
    })
}
fn authority(url: &Url, explicit_port: bool) -> String {
    let host = url.host().expect("validated host");
    if explicit_port || url.port().is_some() {
        format!(
            "{host}:{}",
            url.port_or_known_default().expect("validated scheme")
        )
    } else {
        host.to_string()
    }
}
fn origin(url: &Url) -> (&str, Option<url::Host<&str>>, Option<u16>) {
    (url.scheme(), url.host(), url.port_or_known_default())
}
fn decode(value: &str) -> anyhow::Result<String> {
    Ok(percent_decode_str(value)
        .decode_utf8()
        .context("credentials are not UTF-8")?
        .into_owned())
}
async fn first_address(address: &str) -> anyhow::Result<SocketAddr> {
    lookup_host(address)
        .await?
        .next()
        .context("TCP host resolved to no addresses")
}
fn has_token(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|h| h.split(','))
        .any(|h| h.trim().eq_ignore_ascii_case(token))
}
fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn certificates(path: &PathBuf) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let certificates = rustls_pemfile::certs(&mut BufReader::new(
        File::open(path).with_context(|| format!("open {}", path.display()))?,
    ))
    .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        !certificates.is_empty(),
        "{} contains no PEM certificates",
        path.display()
    );
    Ok(certificates)
}
fn tls_connector(args: &Args, client_identity: bool) -> anyhow::Result<TlsConnector> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = &args.ca {
        for cert in certificates(path)? {
            roots.add(cert)?;
        }
    }
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots);
    let mut config = if client_identity {
        if let (Some(cert), Some(key)) = (&args.cert, &args.key) {
            let key = rustls_pemfile::private_key(&mut BufReader::new(File::open(key)?))?
                .context("no unencrypted PEM private key found")?;
            builder.with_client_auth_cert(certificates(cert)?, key)?
        } else {
            builder.with_no_client_auth()
        }
    } else {
        builder.with_no_client_auth()
    };
    if args.no_check {
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(NoCertificateVerification(provider)));
    }
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsConnector::from(Arc::new(config)))
}

#[derive(Debug)]
struct NoCertificateVerification(Arc<rustls::crypto::CryptoProvider>);
impl rustls::client::danger::ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

// The default yawc backend uses a 15-bit compression window. Do not offer
// client_max_window_bits, since that would permit a smaller outgoing window.
fn parse_extensions(values: &[&str]) -> anyhow::Result<Option<String>> {
    if values.is_empty() {
        return Ok(None);
    }
    ensure!(values.len() == 1, "server returned multiple extensions");
    let mut parts = values[0].split(';').map(str::trim);
    ensure!(
        parts.next() == Some("permessage-deflate"),
        "server returned an unoffered extension"
    );
    let mut seen = std::collections::HashSet::new();
    let mut normalized = String::from("permessage-deflate");
    for part in parts {
        let (name, value) = part
            .split_once('=')
            .map_or((part, None), |(k, v)| (k.trim(), Some(v.trim())));
        ensure!(seen.insert(name), "duplicate compression parameter: {name}");
        match name {
            "client_no_context_takeover" | "server_no_context_takeover" => {
                ensure!(value.is_none(), "invalid compression parameter: {name}");
                normalized.push_str(&format!("; {name}"));
            }
            "server_max_window_bits" => {
                let value = value.context("server_max_window_bits requires a value")?;
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value);
                let bits = value.parse::<u8>().context("invalid compression window")?;
                ensure!((8..=15).contains(&bits), "invalid compression window");
                normalized.push_str(&format!("; {name}={bits}"));
            }
            _ => bail!("server returned an unoffered compression parameter: {name}"),
        }
    }
    Ok(Some(normalized))
}
