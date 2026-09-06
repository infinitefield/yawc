//! SOCKS5 (RFC 1928) client support for outgoing connections.
//!
//! A [`Proxy`] handed to the connection builder makes the client dial the proxy and ask
//! it to open a tunnel to the WebSocket host.
//! Everything above that, TLS included, then runs end to end through the tunnel, so the
//! proxy sees only ciphertext for a `wss://` URL.

use std::{fmt, net::IpAddr};

use percent_encoding::percent_decode_str;
use thiserror::Error;
use url::{Host, Url};

/// The port a SOCKS5 proxy is assumed to listen on when the URL does not say.
const DEFAULT_PORT: u16 = 1080;

/// RFC 1929 gives the username and password a single length byte each.
const MAX_CREDENTIAL_LEN: usize = 255;

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
    pub fn socks5(url: Url) -> Result<Self, Socks5Error> {
        let remote_dns = match url.scheme() {
            "socks5" => false,
            "socks5h" => true,
            scheme => return Err(Socks5Error::UnsupportedScheme(scheme.to_string())),
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
            None => return Err(Socks5Error::MissingHost),
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
                return Err(Socks5Error::CredentialsTooLong);
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
fn decode(value: &str) -> Result<String, Socks5Error> {
    percent_decode_str(value)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .map_err(|_| Socks5Error::InvalidCredentials)
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(matches!(err, Socks5Error::UnsupportedScheme(_)));
    }

    #[test]
    fn rejects_credentials_that_do_not_fit_rfc_1929() {
        let long = "u".repeat(256);
        let url: Url = format!("socks5h://{long}:pass@127.0.0.1:1080")
            .parse()
            .expect("url");
        assert!(matches!(
            Proxy::socks5(url).unwrap_err(),
            Socks5Error::CredentialsTooLong
        ));
    }

    #[test]
    fn debug_never_prints_the_password() {
        let printed = format!("{:?}", proxy("socks5h://user:hunter2@127.0.0.1:1080"));
        assert!(!printed.contains("hunter2"), "{printed}");
    }
}
