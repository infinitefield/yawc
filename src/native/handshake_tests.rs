use std::mem::discriminant;

use hyper::{header, Response, StatusCode};

use super::{Negotiation, Options};
use crate::{Result, WebSocketError};

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

fn upgrade() -> Response<&'static str> {
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "WeBsOcKeT")
        .header(header::CONNECTION, "uPgRaDe")
        .header(header::SEC_WEBSOCKET_ACCEPT, ACCEPT)
        .body("")
        .unwrap()
}

fn verify_clients(response: Response<&'static str>, options: Options) -> Vec<Result<Negotiation>> {
    let results = vec![super::verify(&response, KEY, options.clone())];
    #[cfg(feature = "reqwest")]
    let results = {
        let mut results = results;
        let response = reqwest::Response::from(response);
        results.push(super::verify_upgrade(
            response.status(),
            response.headers(),
            KEY,
            &options,
        ));
        results
    };
    results
}

#[test]
fn both_clients_accept_case_insensitive_upgrade_headers() {
    for result in verify_clients(upgrade(), Options::default()) {
        assert!(result.is_ok());
    }
}

#[test]
fn both_clients_accept_connection_token_lists() {
    for lines in [
        &["keep-alive, Upgrade"][..],
        &["keep-alive", "\tUpGrAdE "][..],
    ] {
        let mut response = upgrade();
        response.headers_mut().remove(header::CONNECTION);
        for value in lines {
            response
                .headers_mut()
                .append(header::CONNECTION, value.parse().unwrap());
        }
        for result in verify_clients(response, Options::default()) {
            assert!(result.is_ok());
        }
    }
}

#[test]
fn both_clients_reject_missing_and_invalid_headers() {
    for (name, expected) in [
        (header::UPGRADE, WebSocketError::InvalidUpgradeHeader),
        (header::CONNECTION, WebSocketError::InvalidConnectionHeader),
        (
            header::SEC_WEBSOCKET_ACCEPT,
            WebSocketError::InvalidUpgradeHeader,
        ),
    ] {
        for value in [None, Some(""), Some("invalid")] {
            let mut response = upgrade();
            match value {
                Some(value) => response
                    .headers_mut()
                    .insert(name.clone(), value.parse().unwrap()),
                None => response.headers_mut().remove(&name),
            };
            for result in verify_clients(response, Options::default()) {
                let Err(err) = result else {
                    panic!("invalid headers must fail the handshake");
                };
                assert_eq!(
                    discriminant(&err),
                    discriminant(&expected),
                    "{name}: {value:?}"
                );
            }
        }
    }
}

#[test]
fn both_clients_reject_an_unsuccessful_status_before_checking_headers() {
    let response = Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body("")
        .unwrap();
    for result in verify_clients(response, Options::default()) {
        assert!(matches!(
            result,
            Err(WebSocketError::InvalidStatusCode(400))
        ));
    }
}

#[test]
fn both_clients_negotiate_compression_and_reject_unoffered_extensions() {
    for offered in [false, true] {
        let mut response = upgrade();
        response.headers_mut().insert(
            header::SEC_WEBSOCKET_EXTENSIONS,
            "permessage-deflate".parse().unwrap(),
        );
        let options = if offered {
            Options::default().with_balanced_compression()
        } else {
            Options::default().without_compression()
        };
        for result in verify_clients(response, options) {
            if offered {
                assert!(result.unwrap().compression.is_some());
            } else {
                assert!(matches!(
                    result,
                    Err(WebSocketError::CompressionNotSupported)
                ));
            }
        }
    }
}

#[test]
fn hyper_preserves_redirect_details() {
    let response = Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, "/next")
        .body("")
        .unwrap();
    assert!(matches!(
        super::verify(&response, KEY, Options::default()),
        Err(WebSocketError::Redirected { status_code: 302, location }) if location == "/next"
    ));
    #[cfg(feature = "reqwest")]
    {
        let response = reqwest::Response::from(response);
        assert!(matches!(
            super::verify_upgrade(
                response.status(),
                response.headers(),
                KEY,
                &Options::default()
            ),
            Err(WebSocketError::InvalidStatusCode(302))
        ));
    }
}
