//! The permessage-deflate offers in a client's `Sec-WebSocket-Extensions` header.
//!
//! A client lists the extensions it wants and may offer permessage-deflate several times
//! with different parameters, most preferred first (RFC 7692, Section 5). The list can be
//! split across several header lines (RFC 6455, Section 9.1). The server answers the first
//! offer it can honor, and must decline an offer with a parameter RFC 7692 does not define
//! for offers, a parameter given twice, or a value the parameter does not allow
//! (Section 7.1).
use std::borrow::Cow;

use super::{WebSocketExtensions, NEGOTIABLE_WINDOW_BITS, PERMESSAGE_DEFLATE};

impl WebSocketExtensions {
    /// Reads the permessage-deflate offers in a client's handshake, most preferred first.
    ///
    /// Offers that RFC 7692 has the server decline are left out, as are other extensions.
    /// A header that does not follow the grammar of RFC 6455, Section 9.1 offers nothing,
    /// and the connection proceeds without extensions, as with
    /// [`WebSocketExtensions::from_headers`].
    pub(crate) fn offers(headers: &hyper::HeaderMap) -> Vec<Self> {
        let mut offers = Vec::new();
        for value in headers.get_all(hyper::header::SEC_WEBSOCKET_EXTENSIONS) {
            let Some(extensions) = value.to_str().ok().and_then(parse) else {
                return Vec::new();
            };
            offers.extend(
                extensions
                    .iter()
                    .filter(|extension| extension.name == PERMESSAGE_DEFLATE)
                    .filter_map(|extension| offer(&extension.params)),
            );
        }
        offers
    }
}

/// An extension named in the header, with its parameters in order.
struct Extension<'a> {
    name: &'a str,
    params: Vec<(&'a str, Option<Cow<'a, str>>)>,
}

/// Splits a header value into extensions, following `extension-list` in RFC 6455,
/// Section 9.1, with optional whitespace around the separators.
///
/// Returns `None` if the value does not follow the grammar.
fn parse(value: &str) -> Option<Vec<Extension<'_>>> {
    let mut input = Input(value);
    let mut extensions = Vec::new();

    loop {
        input.skip_whitespace();
        // A list may hold empty elements, which recipients skip (RFC 7230, Section 7).
        if input.eat(b',') {
            continue;
        }
        if input.is_empty() {
            break;
        }

        let name = input.token()?;
        let mut params = Vec::new();
        loop {
            input.skip_whitespace();
            if !input.eat(b';') {
                break;
            }
            input.skip_whitespace();
            let key = input.token()?;
            input.skip_whitespace();
            let value = if input.eat(b'=') {
                input.skip_whitespace();
                Some(input.value()?)
            } else {
                None
            };
            params.push((key, value));
        }
        extensions.push(Extension { name, params });

        if !input.is_empty() && !input.eat(b',') {
            return None;
        }
    }

    Some(extensions)
}

/// Reads one permessage-deflate offer, or returns `None` if RFC 7692, Section 7.1 has the
/// server decline it.
fn offer(params: &[(&str, Option<Cow<'_, str>>)]) -> Option<WebSocketExtensions> {
    let mut offer = WebSocketExtensions::default();

    for (index, (name, value)) in params.iter().enumerate() {
        if params[..index].iter().any(|(earlier, _)| earlier == name) {
            return None;
        }
        match (*name, value.as_deref()) {
            ("server_no_context_takeover", None) => offer.server_no_context_takeover = true,
            ("client_no_context_takeover", None) => offer.client_no_context_takeover = true,
            ("server_max_window_bits", Some(value)) => {
                offer.server_max_window_bits = Some(Some(window_bits(value)?));
            }
            // Without a value, the client says it can keep to a limit the server picks
            // (Section 7.1.2.2).
            ("client_max_window_bits", None) => offer.client_max_window_bits = Some(None),
            ("client_max_window_bits", Some(value)) => {
                offer.client_max_window_bits = Some(Some(window_bits(value)?));
            }
            _ => return None,
        }
    }

    Some(offer)
}

/// A window size as RFC 7692 writes it: a decimal from 8 to 15 with no leading zeros.
fn window_bits(value: &str) -> Option<u8> {
    let bits: u8 = value.parse().ok()?;
    // `parse` also takes a leading `+` and leading zeros, which the grammar does not.
    (NEGOTIABLE_WINDOW_BITS.contains(&bits) && bits.to_string() == value).then_some(bits)
}

/// What is left of a header value to parse.
struct Input<'a>(&'a str);

impl<'a> Input<'a> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn skip_whitespace(&mut self) {
        self.0 = self.0.trim_start_matches([' ', '\t']);
    }

    /// Consumes `byte` if the input starts with it.
    fn eat(&mut self, byte: u8) -> bool {
        let found = self.0.as_bytes().first() == Some(&byte);
        if found {
            self.0 = &self.0[1..];
        }
        found
    }

    /// Consumes a `token` (RFC 7230, Section 3.2.6).
    fn token(&mut self) -> Option<&'a str> {
        let len = self.0.bytes().take_while(|&byte| is_tchar(byte)).count();
        if len == 0 {
            return None;
        }
        let (token, rest) = self.0.split_at(len);
        self.0 = rest;
        Some(token)
    }

    /// Consumes a parameter value: a `token`, or a `quoted-string` with its escapes
    /// removed (RFC 7230, Section 3.2.6).
    fn value(&mut self) -> Option<Cow<'a, str>> {
        if !self.eat(b'"') {
            return self.token().map(Cow::Borrowed);
        }

        let rest = self.0;
        let mut value = String::new();
        let mut chars = rest.char_indices();
        while let Some((index, c)) = chars.next() {
            match c {
                '"' => {
                    self.0 = &rest[index + 1..];
                    return Some(Cow::Owned(value));
                }
                '\\' => value.push(chars.next()?.1),
                c => value.push(c),
            }
        }

        // The closing quote is missing.
        None
    }
}

/// Whether `byte` may appear in a `token` (`tchar`, RFC 7230, Section 3.2.6).
fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

#[cfg(test)]
mod tests {
    use hyper::{header::SEC_WEBSOCKET_EXTENSIONS, HeaderMap};

    use super::*;
    use crate::DeflateOptions;

    fn offers(lines: &[&str]) -> Vec<WebSocketExtensions> {
        let mut headers = HeaderMap::new();
        for line in lines {
            headers.append(SEC_WEBSOCKET_EXTENSIONS, line.parse().unwrap());
        }
        WebSocketExtensions::offers(&headers)
    }

    /// The offers read from `lines`, each written as the header would carry it.
    fn read(lines: &[&str]) -> Vec<String> {
        offers(lines).iter().map(ToString::to_string).collect()
    }

    /// What a server with `options` answers to `lines`.
    fn answer(options: &DeflateOptions, lines: &[&str]) -> Option<String> {
        WebSocketExtensions::agree(Some(options), &offers(lines)).map(|answer| answer.to_string())
    }

    #[test]
    fn reads_every_offer_in_order() {
        assert_eq!(
            read(&["permessage-deflate; server_max_window_bits=10, permessage-deflate"]),
            [
                "permessage-deflate; server_max_window_bits=10",
                "permessage-deflate"
            ],
        );
    }

    #[test]
    fn skips_other_extensions() {
        assert_eq!(
            read(&["x-webkit-deflate-frame, permessage-deflate"]),
            ["permessage-deflate"],
        );
        assert_eq!(
            read(&["permessage-deflate; client_max_window_bits, foo; bar=1"]),
            ["permessage-deflate; client_max_window_bits"],
        );
    }

    #[test]
    fn reads_offers_across_header_lines() {
        assert_eq!(
            read(&[
                "x-webkit-deflate-frame",
                "permessage-deflate; client_no_context_takeover",
            ]),
            ["permessage-deflate; client_no_context_takeover"],
        );
    }

    #[test]
    fn reads_quoted_values() {
        assert_eq!(
            read(&[r#"permessage-deflate; client_max_window_bits="10""#]),
            ["permessage-deflate; client_max_window_bits=10"],
        );
        assert_eq!(
            read(&[r#"permessage-deflate; server_max_window_bits="1\2""#]),
            ["permessage-deflate; server_max_window_bits=12"],
        );
    }

    #[test]
    fn reads_every_window_size_rfc_7692_allows() {
        for bits in 8..=15 {
            let offer = format!(
                "permessage-deflate; server_max_window_bits={bits}; client_max_window_bits={bits}"
            );
            assert_eq!(read(&[&offer]), std::slice::from_ref(&offer));
        }
    }

    #[test]
    fn allows_whitespace_and_empty_list_elements() {
        assert_eq!(
            read(&[" , permessage-deflate ;\tserver_no_context_takeover , ,"]),
            ["permessage-deflate; server_no_context_takeover"],
        );
        assert_eq!(
            read(&["permessage-deflate; server_max_window_bits = 12"]),
            ["permessage-deflate; server_max_window_bits=12"],
        );
    }

    #[test]
    fn leaves_out_offers_the_server_must_decline() {
        for offer in [
            // Parameters RFC 7692 does not define for offers.
            "permessage-deflate; foo",
            "permessage-deflate; foo=1",
            // A parameter given twice.
            "permessage-deflate; server_no_context_takeover; server_no_context_takeover",
            "permessage-deflate; server_max_window_bits=10; server_max_window_bits=12",
            // Values the parameters do not allow.
            "permessage-deflate; server_no_context_takeover=1",
            "permessage-deflate; client_no_context_takeover=\"\"",
            "permessage-deflate; server_max_window_bits",
            "permessage-deflate; server_max_window_bits=7",
            "permessage-deflate; server_max_window_bits=16",
            "permessage-deflate; client_max_window_bits=08",
            "permessage-deflate; client_max_window_bits=+9",
            "permessage-deflate; client_max_window_bits=nine",
        ] {
            assert!(offers(&[offer]).is_empty(), "{offer}");
        }
    }

    #[test]
    fn a_declined_offer_leaves_the_next_one() {
        assert_eq!(
            read(&["permessage-deflate; foo, permessage-deflate; client_max_window_bits"]),
            ["permessage-deflate; client_max_window_bits"],
        );
    }

    #[test]
    fn a_malformed_header_offers_nothing() {
        for lines in [
            &[r#"permessage-deflate; client_max_window_bits="10"#][..],
            &["permessage-deflate; client_max_window_bits="],
            &["permessage-deflate client_no_context_takeover"],
            &["permessage-deflate;"],
            &["permessage-deflate", "; server_no_context_takeover"],
        ] {
            assert!(offers(lines).is_empty(), "{lines:?}");
        }
    }

    #[test]
    fn answers_nothing_without_an_offer_it_can_take() {
        let options = DeflateOptions::default();
        assert_eq!(answer(&options, &[]), None);
        assert_eq!(answer(&options, &["x-webkit-deflate-frame"]), None);
        assert_eq!(answer(&options, &["permessage-deflate; foo"]), None);
        // Compression disabled on the server.
        assert!(WebSocketExtensions::agree(None, &offers(&["permessage-deflate"])).is_none());
    }

    #[test]
    fn answers_the_first_offer_of_several() {
        let options = DeflateOptions::default();
        assert_eq!(
            answer(
                &options,
                &["permessage-deflate; server_no_context_takeover, permessage-deflate"],
            )
            .as_deref(),
            Some("permessage-deflate; server_no_context_takeover"),
        );
    }

    #[test]
    fn does_not_limit_a_client_window_the_offer_did_not_mention() {
        let options = DeflateOptions::default();
        #[cfg(feature = "zlib")]
        let options = DeflateOptions {
            client_max_window_bits: Some(12),
            ..options
        };
        assert_eq!(
            answer(&options, &["permessage-deflate"]).as_deref(),
            Some("permessage-deflate"),
        );
    }

    /// Autobahn's case 13.7 offers these three, most preferred first.
    const AUTOBAHN_13_7: &str = "permessage-deflate; client_no_context_takeover; \
        client_max_window_bits; server_no_context_takeover; server_max_window_bits=9, \
        permessage-deflate; client_no_context_takeover; client_max_window_bits; \
        server_no_context_takeover, \
        permessage-deflate; client_no_context_takeover; client_max_window_bits";

    #[cfg(feature = "zlib")]
    #[test]
    fn declines_a_window_zlib_cannot_compress_with() {
        let options = DeflateOptions::default();
        // zlib's raw deflate has no 8-bit window.
        assert_eq!(
            answer(&options, &["permessage-deflate; server_max_window_bits=8"]),
            None
        );
        assert_eq!(
            answer(
                &options,
                &["permessage-deflate; server_max_window_bits=8, \
                  permessage-deflate; server_max_window_bits=9"],
            )
            .as_deref(),
            Some("permessage-deflate; server_max_window_bits=9"),
        );
        assert_eq!(
            answer(&options, &[AUTOBAHN_13_7]).as_deref(),
            Some(
                "permessage-deflate; server_max_window_bits=9; client_max_window_bits=9; \
                 server_no_context_takeover; client_no_context_takeover"
            ),
        );
    }

    #[cfg(not(feature = "zlib"))]
    #[test]
    fn declines_a_window_the_default_backend_cannot_compress_with() {
        let options = DeflateOptions::default();
        // The default backend always compresses with the full 32 KiB window.
        assert_eq!(
            answer(&options, &["permessage-deflate; server_max_window_bits=14"]),
            None
        );
        assert_eq!(
            answer(
                &options,
                &["permessage-deflate; server_max_window_bits=14, \
                  permessage-deflate; server_max_window_bits=15"],
            )
            .as_deref(),
            Some("permessage-deflate; server_max_window_bits=15"),
        );
        assert_eq!(
            answer(&options, &[AUTOBAHN_13_7]).as_deref(),
            Some("permessage-deflate; server_no_context_takeover; client_no_context_takeover"),
        );
    }
}
