//! SOCKS5 (RFC 1928) client support for outgoing connections.
//!
//! A [`Proxy`] handed to the connection builder makes the client dial the proxy and ask
//! it to open a tunnel to the WebSocket host.
//! Everything above that, TLS included, then runs end to end through the tunnel, so the
//! proxy sees only ciphertext for a `wss://` URL.

use std::{
    fmt, io,
    net::{IpAddr, SocketAddr},
};

use percent_encoding::percent_decode_str;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{lookup_host, TcpStream},
};
use url::{Host, Url};

use crate::Result;

/// The protocol version this module speaks.
const VERSION: u8 = 5;

/// `CONNECT`, the only command a WebSocket client needs.
const CMD_CONNECT: u8 = 1;

/// The username/password sub-negotiation is versioned separately (RFC 1929).
const AUTH_VERSION: u8 = 1;

/// The port a SOCKS5 proxy is assumed to listen on when the URL does not say.
const DEFAULT_PORT: u16 = 1080;

/// RFC 1929 gives the username and password a single length byte each, and a `CONNECT`
/// request gives the hostname one too.
const MAX_CREDENTIAL_LEN: usize = 255;
const MAX_HOSTNAME_LEN: usize = 255;

/// An authentication method, as offered in the greeting and picked by the proxy.
///
/// Only the two this client can actually run are named; anything else the proxy answers
/// with is reported as unsupported rather than mapped to a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Method {
    /// No authentication.
    None = 0x00,
    /// Username and password, as defined by RFC 1929.
    UserPass = 0x02,
    /// The proxy's answer when it accepts none of the offered methods.
    Unacceptable = 0xFF,
}

impl Method {
    /// Reads back a method the proxy selected, if it is one this client offered.
    fn from_code(code: u8) -> Option<Self> {
        match code {
            0x00 => Some(Self::None),
            0x02 => Some(Self::UserPass),
            0xFF => Some(Self::Unacceptable),
            _ => None,
        }
    }
}

/// The address types a request and a reply can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum AddressType {
    /// Four bytes of IPv4 address.
    Ipv4 = 1,
    /// A length byte followed by that many bytes of hostname.
    Domain = 3,
    /// Sixteen bytes of IPv6 address.
    Ipv6 = 4,
}

impl AddressType {
    /// Reads back an address type, if it is one the protocol defines.
    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Ipv4),
            3 => Some(Self::Domain),
            4 => Some(Self::Ipv6),
            _ => None,
        }
    }
}

/// A SOCKS5 proxy to dial through.
///
/// Build one from a URL with [`Proxy::socks5`]. Both scheme spellings are accepted and
/// they differ in who resolves the WebSocket host: `socks5h://` sends the hostname to the
/// proxy, `socks5://` resolves it locally and sends an address.
///
/// ```
/// use yawc::Proxy;
///
/// # fn main() -> yawc::Result<()> {
/// let proxy = Proxy::socks5("socks5h://user:pass@127.0.0.1:1080".parse()?)?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Proxy {
    /// Where the proxy listens.
    pub(crate) host: Host<String>,
    /// The proxy's port, defaulting to 1080.
    pub(crate) port: u16,
    /// Credentials for RFC 1929 username/password authentication, if the proxy wants any.
    pub(crate) auth: Option<Auth>,
    /// Whether the proxy resolves the target hostname, rather than this client.
    pub(crate) remote_dns: bool,
}

/// RFC 1929 username/password credentials.
#[derive(Debug, Clone)]
pub(crate) struct Auth {
    pub(crate) username: String,
    pub(crate) password: Password,
}

/// A password that stays out of logs.
///
/// `Proxy` derives `Debug` so it can be printed while debugging a connection, and this
/// keeps that from spilling the credential.
#[derive(Clone)]
pub(crate) struct Password(pub(crate) String);

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(***)")
    }
}

impl Proxy {
    /// Parses a `socks5://` or `socks5h://` URL into a proxy definition.
    ///
    /// Credentials come from the URL's userinfo and are percent-decoded, so a password
    /// containing `@` or `:` can be written as `%40` and `%3A`. A URL without a port uses
    /// 1080.
    ///
    /// Prefer `socks5h://` unless the proxy cannot resolve the target itself. A `CONNECT`
    /// request carries a single address, so `socks5://` sends only the first one the
    /// resolver returns, and on a dual-stack host that can be an address the proxy has no
    /// route to.
    pub fn socks5(url: Url) -> Result<Self> {
        let remote_dns = match url.scheme() {
            "socks5" => false,
            "socks5h" => true,
            scheme => return Err(Socks5Error::UnsupportedScheme(scheme.to_string()).into()),
        };

        let host = match url.host() {
            // A non-special scheme leaves an IPv4 literal as a domain, and which address
            // type the request carries depends on telling them apart.
            Some(Host::Domain(domain)) => match domain.parse::<IpAddr>() {
                Ok(IpAddr::V4(ip)) => Host::Ipv4(ip),
                Ok(IpAddr::V6(ip)) => Host::Ipv6(ip),
                Err(_) => Host::Domain(domain.to_string()),
            },
            Some(Host::Ipv4(ip)) => Host::Ipv4(ip),
            Some(Host::Ipv6(ip)) => Host::Ipv6(ip),
            None => return Err(Socks5Error::MissingHost.into()),
        };

        let auth = match decode(url.username())? {
            username if username.is_empty() => None,
            username => Some(Auth {
                username,
                password: Password(decode(url.password().unwrap_or_default())?),
            }),
        };

        if let Some(auth) = auth.as_ref() {
            if auth.username.len() > MAX_CREDENTIAL_LEN
                || auth.password.0.len() > MAX_CREDENTIAL_LEN
            {
                return Err(Socks5Error::CredentialsTooLong.into());
            }
        }

        Ok(Self {
            host,
            port: url.port().unwrap_or(DEFAULT_PORT),
            auth,
            remote_dns,
        })
    }
}

/// Percent-decodes one half of a URL's userinfo.
fn decode(value: &str) -> Result<String> {
    percent_decode_str(value)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .map_err(|_| Socks5Error::InvalidCredentials.into())
}

/// Where the proxy is asked to connect to.
///
/// A hostname is what `socks5h://` sends, leaving resolution to the proxy. An address is
/// what `socks5://` sends, and what a caller who pinned the address gets either way.
pub(crate) enum Target {
    /// Resolved by the proxy.
    Domain { host: String, port: u16 },
    /// Resolved by this client.
    Addr(SocketAddr),
}

/// Opens a tunnel to `target` over an already-connected stream to the proxy.
///
/// On return the stream carries nothing but the tunnel, so TLS and the WebSocket
/// handshake run over it exactly as they would over a direct socket.
pub(crate) async fn connect<S>(stream: &mut S, proxy: &Proxy, target: &Target) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // A domain goes into a single length byte, so an oversized one cannot be asked for at
    // all. Checking before the greeting keeps a doomed request off the wire.
    if let Target::Domain { host, .. } = target {
        if host.len() > MAX_HOSTNAME_LEN {
            return Err(Socks5Error::HostnameTooLong.into());
        }
    }

    negotiate(stream, proxy).await?;
    send_request(stream, target).await?;
    read_reply(stream).await
}

/// Offers the methods this client supports and runs whichever one the proxy picks.
async fn negotiate<S>(stream: &mut S, proxy: &Proxy) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let greeting: &[u8] = match proxy.auth {
        Some(_) => &[VERSION, 2, Method::None as u8, Method::UserPass as u8],
        None => &[VERSION, 1, Method::None as u8],
    };
    stream.write_all(greeting).await?;
    stream.flush().await?;

    let mut chosen = [0; 2];
    stream.read_exact(&mut chosen).await?;

    if chosen[0] != VERSION {
        return Err(Socks5Error::UnsupportedVersion(chosen[0]).into());
    }

    match (Method::from_code(chosen[1]), proxy.auth.as_ref()) {
        (Some(Method::None), _) => Ok(()),
        (Some(Method::UserPass), Some(auth)) => authenticate(stream, auth).await,
        (Some(Method::Unacceptable), _) => Err(Socks5Error::NoAcceptableAuth.into()),
        // Either a method that was never offered, or username/password without any
        // credentials to send.
        _ => Err(Socks5Error::UnsupportedMethod(chosen[1]).into()),
    }
}

/// Runs the RFC 1929 username/password sub-negotiation.
async fn authenticate<S>(stream: &mut S, auth: &Auth) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let username = auth.username.as_bytes();
    let password = auth.password.0.as_bytes();

    let mut request = Vec::with_capacity(3 + username.len() + password.len());
    request.push(AUTH_VERSION);
    request.push(username.len() as u8);
    request.extend_from_slice(username);
    request.push(password.len() as u8);
    request.extend_from_slice(password);

    stream.write_all(&request).await?;
    stream.flush().await?;

    let mut status = [0; 2];
    stream.read_exact(&mut status).await?;

    if status[0] != AUTH_VERSION {
        return Err(Socks5Error::UnsupportedVersion(status[0]).into());
    }
    if status[1] != 0 {
        return Err(Socks5Error::AuthFailed(status[1]).into());
    }

    Ok(())
}

/// Sends the `CONNECT` request for the target.
async fn send_request<S>(stream: &mut S, target: &Target) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut request = vec![VERSION, CMD_CONNECT, 0];

    match target {
        Target::Domain { host, port } => {
            request.push(AddressType::Domain as u8);
            request.push(host.len() as u8);
            request.extend_from_slice(host.as_bytes());
            request.extend_from_slice(&port.to_be_bytes());
        }
        Target::Addr(SocketAddr::V4(addr)) => {
            request.push(AddressType::Ipv4 as u8);
            request.extend_from_slice(&addr.ip().octets());
            request.extend_from_slice(&addr.port().to_be_bytes());
        }
        Target::Addr(SocketAddr::V6(addr)) => {
            request.push(AddressType::Ipv6 as u8);
            request.extend_from_slice(&addr.ip().octets());
            request.extend_from_slice(&addr.port().to_be_bytes());
        }
    }

    stream.write_all(&request).await?;
    stream.flush().await?;

    Ok(())
}

/// Reads the reply, including the bound address, so the stream is left at the first byte
/// the target itself sent.
async fn read_reply<S>(stream: &mut S) -> Result<()>
where
    S: AsyncRead + Unpin,
{
    let mut head = [0; 4];
    stream.read_exact(&mut head).await?;

    if head[0] != VERSION {
        return Err(Socks5Error::UnsupportedVersion(head[0]).into());
    }
    if head[1] != 0 {
        return Err(Socks5Error::Rejected(ReplyCode::from(head[1])).into());
    }

    let address_len = match AddressType::from_code(head[3]) {
        Some(AddressType::Ipv4) => 4,
        Some(AddressType::Ipv6) => 16,
        Some(AddressType::Domain) => {
            let mut len = [0; 1];
            stream.read_exact(&mut len).await?;
            usize::from(len[0])
        }
        None => return Err(Socks5Error::InvalidAddressType(head[3]).into()),
    };

    // The bound address and port are of no use to a client that only wanted a tunnel, but
    // they have to leave the stream so the bytes after them are the target's.
    let mut bound = vec![0; address_len + 2];
    stream.read_exact(&mut bound).await?;

    Ok(())
}

/// The reply code a proxy returns when it refuses a `CONNECT` (RFC 1928 section 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyCode {
    /// General SOCKS server failure.
    GeneralFailure,
    /// Connection not allowed by ruleset.
    NotAllowed,
    /// Network unreachable.
    NetworkUnreachable,
    /// Host unreachable.
    HostUnreachable,
    /// Connection refused by the target.
    ConnectionRefused,
    /// TTL expired.
    TtlExpired,
    /// Command not supported.
    CommandNotSupported,
    /// Address type not supported.
    AddressTypeNotSupported,
    /// A code this version of the protocol does not define.
    Unassigned(u8),
}

impl From<u8> for ReplyCode {
    fn from(code: u8) -> Self {
        match code {
            1 => Self::GeneralFailure,
            2 => Self::NotAllowed,
            3 => Self::NetworkUnreachable,
            4 => Self::HostUnreachable,
            5 => Self::ConnectionRefused,
            6 => Self::TtlExpired,
            7 => Self::CommandNotSupported,
            8 => Self::AddressTypeNotSupported,
            code => Self::Unassigned(code),
        }
    }
}

impl fmt::Display for ReplyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GeneralFailure => f.write_str("general failure"),
            Self::NotAllowed => f.write_str("connection not allowed by ruleset"),
            Self::NetworkUnreachable => f.write_str("network unreachable"),
            Self::HostUnreachable => f.write_str("host unreachable"),
            Self::ConnectionRefused => f.write_str("connection refused"),
            Self::TtlExpired => f.write_str("ttl expired"),
            Self::CommandNotSupported => f.write_str("command not supported"),
            Self::AddressTypeNotSupported => f.write_str("address type not supported"),
            Self::Unassigned(code) => write!(f, "unassigned reply code {code}"),
        }
    }
}

impl Proxy {
    /// Connects to the proxy itself.
    pub(crate) async fn dial(&self) -> Result<TcpStream> {
        let stream = match &self.host {
            Host::Domain(domain) => TcpStream::connect((domain.as_str(), self.port)).await?,
            Host::Ipv4(ip) => TcpStream::connect(SocketAddr::from((*ip, self.port))).await?,
            Host::Ipv6(ip) => TcpStream::connect(SocketAddr::from((*ip, self.port))).await?,
        };

        Ok(stream)
    }

    /// Works out what the proxy should be asked to connect to.
    ///
    /// An address the caller pinned is used as it stands. Otherwise a hostname is either
    /// passed on for the proxy to resolve, which is what `socks5h://` means, or resolved
    /// here first.
    pub(crate) async fn target(&self, url: &Url, pinned: Option<SocketAddr>) -> Result<Target> {
        if let Some(addr) = pinned {
            return Ok(Target::Addr(addr));
        }

        let port = url.port_or_known_default().expect("port");

        match url.host().expect("hostname") {
            Host::Ipv4(ip) => Ok(Target::Addr(SocketAddr::from((ip, port)))),
            Host::Ipv6(ip) => Ok(Target::Addr(SocketAddr::from((ip, port)))),
            Host::Domain(domain) if self.remote_dns => Ok(Target::Domain {
                host: domain.to_string(),
                port,
            }),
            Host::Domain(domain) => lookup_host((domain, port))
                .await?
                .next()
                .map(Target::Addr)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "no address for host").into()
                }),
        }
    }
}

/// Everything that can go wrong talking to a SOCKS5 proxy.
#[derive(Debug, Error)]
pub enum Socks5Error {
    /// The proxy URL used a scheme other than `socks5` or `socks5h`.
    #[error("unsupported proxy scheme: {0}")]
    UnsupportedScheme(String),

    /// The proxy URL has no host to connect to.
    #[error("proxy url has no host")]
    MissingHost,

    /// The userinfo in the proxy URL is not valid UTF-8 once percent-decoded.
    #[error("proxy credentials are not valid UTF-8")]
    InvalidCredentials,

    /// RFC 1929 cannot carry a username or password longer than 255 bytes.
    #[error("proxy credentials are longer than 255 bytes")]
    CredentialsTooLong,

    /// A `CONNECT` request cannot carry a hostname longer than 255 bytes.
    #[error("target hostname is longer than 255 bytes")]
    HostnameTooLong,

    /// The proxy answered with a version this client does not speak.
    #[error("proxy replied with unsupported version {0}")]
    UnsupportedVersion(u8),

    /// The proxy rejected every authentication method offered.
    #[error("proxy accepts none of the offered authentication methods")]
    NoAcceptableAuth,

    /// The proxy picked a method that was never offered.
    #[error("proxy selected unsupported authentication method {0}")]
    UnsupportedMethod(u8),

    /// The proxy rejected the credentials.
    #[error("proxy rejected the credentials with status {0}")]
    AuthFailed(u8),

    /// The proxy refused to open the tunnel.
    #[error("proxy refused the connection: {0}")]
    Rejected(ReplyCode),

    /// The reply carried a bound address of an unknown type.
    #[error("proxy replied with unsupported address type {0}")]
    InvalidAddressType(u8),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WebSocketError;
    use std::net::Ipv6Addr;
    use url::{Host, Url};

    fn proxy(url: &str) -> Proxy {
        Proxy::socks5(url.parse().expect("url")).expect("proxy")
    }

    #[test]
    fn socks5h_scheme_asks_the_proxy_to_resolve() {
        assert!(proxy("socks5h://127.0.0.1:1080").remote_dns);
    }

    #[test]
    fn socks5_scheme_resolves_locally() {
        assert!(!proxy("socks5://127.0.0.1:1080").remote_dns);
    }

    #[test]
    fn missing_port_defaults_to_1080() {
        assert_eq!(proxy("socks5h://proxy.example").port, 1080);
    }

    #[test]
    fn host_keeps_its_parsed_form() {
        assert_eq!(
            proxy("socks5h://[::1]:9050").host,
            Host::<String>::Ipv6("::1".parse().expect("addr"))
        );
        assert_eq!(
            proxy("socks5h://proxy.example:1080").host,
            Host::Domain("proxy.example".to_string())
        );
    }

    #[test]
    fn an_ip_literal_is_recognised_as_an_ip_host() {
        assert_eq!(
            proxy("socks5h://127.0.0.1:1080").host,
            Host::<String>::Ipv4("127.0.0.1".parse().expect("addr"))
        );
    }

    #[test]
    fn userinfo_becomes_credentials() {
        let auth = proxy("socks5h://user:p%40ss@127.0.0.1:1080")
            .auth
            .expect("auth");
        assert_eq!(auth.username, "user");
        assert_eq!(auth.password.0, "p@ss");
    }

    #[test]
    fn no_userinfo_means_no_credentials() {
        assert!(proxy("socks5h://127.0.0.1:1080").auth.is_none());
    }

    #[test]
    fn rejects_a_non_socks5_scheme() {
        let err = Proxy::socks5("http://127.0.0.1:8080".parse().expect("url")).unwrap_err();
        assert!(matches!(
            err,
            WebSocketError::Socks5(Socks5Error::UnsupportedScheme(_))
        ));
    }

    #[test]
    fn rejects_credentials_that_do_not_fit_rfc_1929() {
        let long = "u".repeat(256);
        let url: Url = format!("socks5h://{long}:pass@127.0.0.1:1080")
            .parse()
            .expect("url");
        assert!(matches!(
            Proxy::socks5(url).unwrap_err(),
            WebSocketError::Socks5(Socks5Error::CredentialsTooLong)
        ));
    }

    #[test]
    fn debug_never_prints_the_password() {
        let printed = format!("{:?}", proxy("socks5h://user:hunter2@127.0.0.1:1080"));
        assert!(!printed.contains("hunter2"), "{printed}");
    }

    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};

    /// Reads exactly `n` bytes from the fake proxy's end of the pipe.
    async fn read_n(io: &mut DuplexStream, n: usize) -> Vec<u8> {
        let mut buf = vec![0; n];
        io.read_exact(&mut buf).await.expect("read");
        buf
    }

    /// A successful `CONNECT` reply bound to 0.0.0.0:0.
    const OK_REPLY: &[u8] = &[5, 0, 0, 1, 0, 0, 0, 0, 0, 0];

    fn target(host: &str, port: u16) -> Target {
        Target::Domain {
            host: host.to_string(),
            port,
        }
    }

    #[tokio::test]
    async fn a_plain_connect_asks_the_proxy_for_the_target_by_name() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5h://127.0.0.1:1080");

        let fake = tokio::spawn(async move {
            assert_eq!(read_n(&mut server, 3).await, [5, 1, 0]);
            server.write_all(&[5, 0]).await.expect("write");

            let request = read_n(&mut server, 4 + 1 + 11 + 2).await;
            assert_eq!(request, b"\x05\x01\x00\x03\x0bexample.com\x01\xbb");

            server.write_all(OK_REPLY).await.expect("write");
        });

        connect(&mut client, &proxy, &target("example.com", 443))
            .await
            .expect("handshake");
        fake.await.expect("fake proxy");
    }

    #[tokio::test]
    async fn credentials_are_offered_and_sent() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5h://user:pass@127.0.0.1:1080");

        let fake = tokio::spawn(async move {
            assert_eq!(read_n(&mut server, 4).await, [5, 2, 0, 2]);
            server.write_all(&[5, 2]).await.expect("write");

            assert_eq!(read_n(&mut server, 11).await, b"\x01\x04user\x04pass");
            server.write_all(&[1, 0]).await.expect("write");

            let _ = read_n(&mut server, 4 + 1 + 11 + 2).await;
            server.write_all(OK_REPLY).await.expect("write");
        });

        connect(&mut client, &proxy, &target("example.com", 443))
            .await
            .expect("handshake");
        fake.await.expect("fake proxy");
    }

    #[tokio::test]
    async fn a_rejected_password_fails_the_connection() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5h://user:pass@127.0.0.1:1080");

        tokio::spawn(async move {
            let _ = read_n(&mut server, 4).await;
            server.write_all(&[5, 2]).await.expect("write");
            let _ = read_n(&mut server, 11).await;
            server.write_all(&[1, 1]).await.expect("write");
        });

        let err = connect(&mut client, &proxy, &target("example.com", 443))
            .await
            .unwrap_err();
        assert!(
            matches!(err, WebSocketError::Socks5(Socks5Error::AuthFailed(1))),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_proxy_that_accepts_no_method_fails_the_connection() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5h://127.0.0.1:1080");

        tokio::spawn(async move {
            let _ = read_n(&mut server, 3).await;
            server.write_all(&[5, 0xFF]).await.expect("write");
        });

        let err = connect(&mut client, &proxy, &target("example.com", 443))
            .await
            .unwrap_err();
        assert!(
            matches!(err, WebSocketError::Socks5(Socks5Error::NoAcceptableAuth)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_method_that_was_never_offered_fails_the_connection() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5h://127.0.0.1:1080");

        tokio::spawn(async move {
            let _ = read_n(&mut server, 3).await;
            // Username/password, which a proxy without credentials never offered.
            server.write_all(&[5, 2]).await.expect("write");
        });

        let err = connect(&mut client, &proxy, &target("example.com", 443))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                WebSocketError::Socks5(Socks5Error::UnsupportedMethod(2))
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_greeting_from_another_version_fails_the_connection() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5h://127.0.0.1:1080");

        tokio::spawn(async move {
            let _ = read_n(&mut server, 3).await;
            server.write_all(&[4, 0]).await.expect("write");
        });

        let err = connect(&mut client, &proxy, &target("example.com", 443))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                WebSocketError::Socks5(Socks5Error::UnsupportedVersion(4))
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_reply_code_becomes_a_typed_error() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5h://127.0.0.1:1080");

        tokio::spawn(async move {
            let _ = read_n(&mut server, 3).await;
            server.write_all(&[5, 0]).await.expect("write");
            let _ = read_n(&mut server, 4 + 1 + 11 + 2).await;
            server
                .write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .expect("write");
        });

        let err = connect(&mut client, &proxy, &target("example.com", 443))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                WebSocketError::Socks5(Socks5Error::Rejected(ReplyCode::ConnectionRefused))
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_ip_target_is_sent_as_an_address_not_a_name() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5://127.0.0.1:1080");

        let fake = tokio::spawn(async move {
            let _ = read_n(&mut server, 3).await;
            server.write_all(&[5, 0]).await.expect("write");

            assert_eq!(
                read_n(&mut server, 10).await,
                [5, 1, 0, 1, 93, 184, 216, 34, 0x01, 0xbb]
            );
            server.write_all(OK_REPLY).await.expect("write");
        });

        let target = Target::Addr("93.184.216.34:443".parse().expect("addr"));
        connect(&mut client, &proxy, &target)
            .await
            .expect("handshake");
        fake.await.expect("fake proxy");
    }

    #[tokio::test]
    async fn an_ipv6_target_is_sent_as_a_16_byte_address() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5://127.0.0.1:1080");

        let fake = tokio::spawn(async move {
            let _ = read_n(&mut server, 3).await;
            server.write_all(&[5, 0]).await.expect("write");

            let request = read_n(&mut server, 22).await;
            assert_eq!(&request[..4], [5, 1, 0, 4]);
            assert_eq!(&request[4..20], Ipv6Addr::LOCALHOST.octets());
            assert_eq!(&request[20..], [0x01, 0xbb]);

            server.write_all(OK_REPLY).await.expect("write");
        });

        let target = Target::Addr("[::1]:443".parse().expect("addr"));
        connect(&mut client, &proxy, &target)
            .await
            .expect("handshake");
        fake.await.expect("fake proxy");
    }

    #[tokio::test]
    async fn the_bound_address_is_consumed_so_the_tunnel_starts_clean() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5h://127.0.0.1:1080");

        tokio::spawn(async move {
            let _ = read_n(&mut server, 3).await;
            server.write_all(&[5, 0]).await.expect("write");
            let _ = read_n(&mut server, 4 + 1 + 11 + 2).await;

            // A reply bound to an IPv6 address, immediately followed by tunnelled data.
            let mut reply = vec![5, 0, 0, 4];
            reply.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
            reply.extend_from_slice(&[0x01, 0xbb]);
            reply.extend_from_slice(b"first byte of the tunnel");
            server.write_all(&reply).await.expect("write");
        });

        connect(&mut client, &proxy, &target("example.com", 443))
            .await
            .expect("handshake");

        let mut tunnelled = [0; 24];
        client.read_exact(&mut tunnelled).await.expect("read");
        assert_eq!(&tunnelled, b"first byte of the tunnel");
    }

    #[tokio::test]
    async fn a_bound_address_of_an_unknown_type_fails_the_connection() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5h://127.0.0.1:1080");

        tokio::spawn(async move {
            let _ = read_n(&mut server, 3).await;
            server.write_all(&[5, 0]).await.expect("write");
            let _ = read_n(&mut server, 4 + 1 + 11 + 2).await;
            server.write_all(&[5, 0, 0, 9, 0, 0]).await.expect("write");
        });

        let err = connect(&mut client, &proxy, &target("example.com", 443))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                WebSocketError::Socks5(Socks5Error::InvalidAddressType(9))
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_hostname_that_does_not_fit_the_request_is_rejected() {
        let (mut client, _server) = duplex(1024);
        let proxy = proxy("socks5h://127.0.0.1:1080");

        let err = connect(&mut client, &proxy, &target(&"h".repeat(256), 443))
            .await
            .unwrap_err();
        assert!(
            matches!(err, WebSocketError::Socks5(Socks5Error::HostnameTooLong)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_truncated_reply_is_an_io_error() {
        let (mut client, mut server) = duplex(1024);
        let proxy = proxy("socks5h://127.0.0.1:1080");

        tokio::spawn(async move {
            let _ = read_n(&mut server, 3).await;
            server.write_all(&[5, 0]).await.expect("write");
            let _ = read_n(&mut server, 4 + 1 + 11 + 2).await;
            server.write_all(&[5, 0, 0]).await.expect("write");
            drop(server);
        });

        let err = connect(&mut client, &proxy, &target("example.com", 443))
            .await
            .unwrap_err();
        assert!(err.is_io_error(), "{err:?}");
    }
}
