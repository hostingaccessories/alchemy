//! Request checks that keep web pages other than Alchemy's own away from
//! the API.
//!
//! The TCP listener is reachable from any page open in the user's browser.
//! CORS stops such a page reading responses but not sending them: a form
//! can POST cross-origin without a preflight, and a DNS-rebinding page
//! sends its requests as same-origin under a host name it controls. Since
//! mutating routes end in `docker compose up`, which is root by another
//! name, the listener refuses:
//!
//! 1. every request whose `Host` is not a loopback name or address, or the
//!    address the daemon was told to listen on ([`check_host`]);
//! 2. every mutating request whose `Origin` is present and is neither the
//!    request's own origin nor one of [`TAURI_ORIGINS`] ([`check_origin`]);
//! 3. every mutating request whose `Content-Type` is not
//!    `application/json`, with or without a body ([`check_content_type`]).
//!    JSON is not a CORS-safelisted type, so a cross-origin caller must
//!    pass a preflight first, and CORS answers that only for the shell.
//!
//! A request is mutating unless its method is `GET`, `HEAD`, or `OPTIONS`.
//!
//! The Unix socket applies only the content-type check. No browser can
//! open it, its `0o600` mode is the access control, and the CLI sends a
//! `Host` that names nothing. The content-type rule is part of the API
//! contract rather than a browser defence, so both front doors share it.
//!
//! Everything here is pure; the middleware in [`crate::http`] applies it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use alchemy_api::ErrorCode;
use axum::http::uri::Authority;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};

/// Origins the Tauri shell serves its bundled UI from (macOS and Linux
/// respectively). The browser UI is same-origin and needs no CORS.
pub const TAURI_ORIGINS: [&str; 2] = ["tauri://localhost", "http://tauri.localhost"];

/// Host names accepted in `Host` besides IP literals. Browsers resolve
/// `localhost` and its subdomains to loopback themselves, so no DNS answer
/// can point them elsewhere; `tauri.localhost` is the shell's own name.
const LOOPBACK_NAMES: [&str; 2] = ["localhost", "tauri.localhost"];

/// Why a request was refused before reaching a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `Host` is missing, repeated, malformed, or not one the daemon
    /// answers to.
    Host,
    /// A mutating request came from another web origin.
    Origin,
    /// A mutating request did not declare a JSON body.
    ContentType,
}

impl Refusal {
    /// The HTTP status to answer with.
    pub fn status(self) -> StatusCode {
        match self {
            Self::Host | Self::Origin => StatusCode::FORBIDDEN,
            Self::ContentType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        }
    }

    /// The [`ErrorCode`] to answer with.
    pub fn code(self) -> ErrorCode {
        match self {
            Self::Host | Self::Origin => ErrorCode::Forbidden,
            Self::ContentType => ErrorCode::UnsupportedMediaType,
        }
    }

    /// A message safe to show the user. It never echoes the request.
    pub fn message(self) -> &'static str {
        match self {
            Self::Host => {
                "host not allowed: reach alchemyd at localhost, 127.0.0.1, or its listen address"
            }
            Self::Origin => "cross-origin request refused",
            Self::ContentType => "content type must be application/json",
        }
    }
}

/// Whether `method` can change daemon state, and so gets the origin and
/// content-type checks.
pub fn is_mutating(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Refuse a request whose `Host` the daemon, listening on `listen`, does
/// not answer to. See [`host_allowed`].
pub fn check_host(headers: &HeaderMap, listen: IpAddr) -> Result<(), Refusal> {
    match single(headers, &header::HOST) {
        Some(host) if host_allowed(host, listen) => Ok(()),
        _ => Err(Refusal::Host),
    }
}

/// Refuse a mutating request from a web origin other than its own or the
/// Tauri shell's. A request without `Origin` passes: browsers send one on
/// every cross-origin request that is not a `GET` or `HEAD`, so its absence
/// means a same-origin page or a client that is not a browser.
pub fn check_origin(method: &Method, headers: &HeaderMap) -> Result<(), Refusal> {
    if !is_mutating(method) || !headers.contains_key(header::ORIGIN) {
        return Ok(());
    }
    match (
        single(headers, &header::ORIGIN),
        single(headers, &header::HOST),
    ) {
        (Some(origin), Some(host)) if origin_allowed(origin, host) => Ok(()),
        _ => Err(Refusal::Origin),
    }
}

/// Refuse a mutating request whose `Content-Type` is not JSON, whether or
/// not it has a body. See [`is_json`].
pub fn check_content_type(method: &Method, headers: &HeaderMap) -> Result<(), Refusal> {
    if !is_mutating(method) {
        return Ok(());
    }
    match single(headers, &header::CONTENT_TYPE) {
        Some(content_type) if is_json(content_type) => Ok(()),
        _ => Err(Refusal::ContentType),
    }
}

/// Whether a daemon listening on `listen` answers to the `Host` value
/// `host`, with or without a port.
///
/// Accepted: [`LOOPBACK_NAMES`] in any case, loopback IP literals, and
/// `listen` itself. When `listen` is unspecified (`0.0.0.0` or `::`), any
/// IP literal is accepted, since the daemon answers on every address.
/// Rebinding needs a host name the attacker controls, so an IP literal is
/// never one. Any other name is refused, including a LAN name for a daemon
/// deliberately bound off loopback: reach it by address instead.
pub fn host_allowed(host: &str, listen: IpAddr) -> bool {
    if host.contains('@') {
        return false;
    }
    let Ok(authority) = host.parse::<Authority>() else {
        return false;
    };
    let name = authority.host();
    if !host.strip_prefix(name).is_some_and(valid_port) {
        return false;
    }
    if LOOPBACK_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n)) {
        return true;
    }
    let ip = match name.strip_prefix('[').and_then(|n| n.strip_suffix(']')) {
        Some(v6) => v6.parse::<Ipv6Addr>().map(IpAddr::V6),
        None => name.parse::<Ipv4Addr>().map(IpAddr::V4),
    };
    ip.is_ok_and(|ip| ip.is_loopback() || ip == listen || listen.is_unspecified())
}

/// Whether `origin` may send a mutating request with the `Host` value
/// `host`: it is one of [`TAURI_ORIGINS`], or `http` or `https` on exactly
/// that host and port.
pub fn origin_allowed(origin: &str, host: &str) -> bool {
    if TAURI_ORIGINS.contains(&origin) {
        return true;
    }
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .is_some_and(|authority| !authority.is_empty() && authority.eq_ignore_ascii_case(host))
}

/// Whether `content_type` is `application/json`, in any case, with or
/// without parameters such as `charset`. Suffixed types like
/// `application/merge-patch+json` are not accepted.
pub fn is_json(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
}

/// Whether `rest`, what follows the host in a `Host` value, is empty or a
/// `:` and a port number.
fn valid_port(rest: &str) -> bool {
    rest.is_empty()
        || rest.strip_prefix(':').is_some_and(|port| {
            port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok()
        })
}

/// The value of `name` if it appears exactly once and is visible ASCII.
fn single<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => HeaderValue::to_str(value).ok(),
        _ => None,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const LAN: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
    const ANY_V4: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    const ANY_V6: IpAddr = IpAddr::V6(Ipv6Addr::UNSPECIFIED);

    fn headers(pairs: &[(HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn host_allowed_table() {
        let cases = [
            ("localhost", LOOPBACK, true),
            ("localhost:7240", LOOPBACK, true),
            ("LocalHost:5173", LOOPBACK, true),
            ("tauri.localhost", LOOPBACK, true),
            ("127.0.0.1", LOOPBACK, true),
            ("127.0.0.1:7240", LOOPBACK, true),
            ("127.1.2.3:7240", LOOPBACK, true),
            ("[::1]", LOOPBACK, true),
            ("[::1]:7240", LOOPBACK, true),
            ("192.168.1.5:7240", LAN, true),
            ("192.168.1.5:7240", LOOPBACK, false),
            ("localhost:7240", LAN, true),
            ("10.0.0.9:7240", LAN, false),
            ("10.0.0.9:7240", ANY_V4, true),
            ("[fe80::1]:7240", ANY_V6, true),
            ("nas.lan:7240", ANY_V4, false),
            ("nas.lan:7240", LAN, false),
            ("evil.example", LOOPBACK, false),
            ("evil.example:7240", LOOPBACK, false),
            ("localhost.evil.example", LOOPBACK, false),
            ("evil.localhost", LOOPBACK, false),
            ("localhost.", LOOPBACK, false),
            ("user@localhost", LOOPBACK, false),
            ("localhost@evil.example", LOOPBACK, false),
            ("::1", LOOPBACK, false),
            ("[127.0.0.1]", LOOPBACK, false),
            ("localhost:port", LOOPBACK, false),
            ("localhost:", LOOPBACK, false),
            ("localhost:+80", LOOPBACK, false),
            ("localhost:70000", LOOPBACK, false),
            ("localhost/path", LOOPBACK, false),
            ("", LOOPBACK, false),
            ("0.0.0.0:7240", LOOPBACK, false),
        ];
        for (host, listen, want) in cases {
            assert_eq!(host_allowed(host, listen), want, "{host} on {listen}");
        }
    }

    #[test]
    fn origin_allowed_table() {
        let cases = [
            ("tauri://localhost", "127.0.0.1:7240", true),
            ("http://tauri.localhost", "127.0.0.1:7240", true),
            ("http://127.0.0.1:7240", "127.0.0.1:7240", true),
            ("https://127.0.0.1:7240", "127.0.0.1:7240", true),
            ("http://localhost:5173", "localhost:5173", true),
            ("http://LOCALHOST:5173", "localhost:5173", true),
            ("http://[::1]:7240", "[::1]:7240", true),
            ("http://localhost:5173", "127.0.0.1:7240", false),
            ("http://127.0.0.1:7241", "127.0.0.1:7240", false),
            ("http://127.0.0.1", "127.0.0.1:7240", false),
            ("http://127.0.0.1:7240/", "127.0.0.1:7240", false),
            ("https://evil.example", "127.0.0.1:7240", false),
            ("null", "127.0.0.1:7240", false),
            ("file://", "127.0.0.1:7240", false),
            ("http://", "", false),
            ("ftp://127.0.0.1:7240", "127.0.0.1:7240", false),
            ("tauri://LOCALHOST", "127.0.0.1:7240", false),
        ];
        for (origin, host, want) in cases {
            assert_eq!(origin_allowed(origin, host), want, "{origin} on {host}");
        }
    }

    #[test]
    fn is_json_table() {
        let cases = [
            ("application/json", true),
            ("Application/JSON", true),
            ("application/json; charset=utf-8", true),
            ("application/json;charset=UTF-8", true),
            (" application/json ", true),
            ("text/plain", false),
            ("text/plain; application/json", false),
            ("application/x-www-form-urlencoded", false),
            ("multipart/form-data; boundary=x", false),
            ("application/merge-patch+json", false),
            ("application/jsonp", false),
            ("application/json, text/plain", false),
            ("", false),
        ];
        for (content_type, want) in cases {
            assert_eq!(is_json(content_type), want, "{content_type:?}");
        }
    }

    #[test]
    fn mutating_methods() {
        for (method, want) in [
            (Method::GET, false),
            (Method::HEAD, false),
            (Method::OPTIONS, false),
            (Method::POST, true),
            (Method::PUT, true),
            (Method::PATCH, true),
            (Method::DELETE, true),
            (Method::TRACE, true),
        ] {
            assert_eq!(is_mutating(&method), want, "{method}");
        }
    }

    #[test]
    fn check_host_table() {
        let cases = [
            (headers(&[(header::HOST, "127.0.0.1:7240")]), Ok(())),
            (headers(&[]), Err(Refusal::Host)),
            (
                headers(&[(header::HOST, "evil.example")]),
                Err(Refusal::Host),
            ),
            (
                headers(&[(header::HOST, "127.0.0.1"), (header::HOST, "evil.example")]),
                Err(Refusal::Host),
            ),
        ];
        for (map, want) in cases {
            assert_eq!(check_host(&map, LOOPBACK), want, "{map:?}");
        }
        let mut opaque = HeaderMap::new();
        opaque.insert(
            header::HOST,
            HeaderValue::from_bytes(b"local\xffhost").unwrap(),
        );
        assert_eq!(check_host(&opaque, LOOPBACK), Err(Refusal::Host));
    }

    #[test]
    fn check_origin_table() {
        let host = (header::HOST, "127.0.0.1:7240");
        let cases = [
            (Method::POST, headers(std::slice::from_ref(&host)), Ok(())),
            (
                Method::POST,
                headers(&[host.clone(), (header::ORIGIN, "http://127.0.0.1:7240")]),
                Ok(()),
            ),
            (
                Method::DELETE,
                headers(&[host.clone(), (header::ORIGIN, "tauri://localhost")]),
                Ok(()),
            ),
            (
                Method::POST,
                headers(&[host.clone(), (header::ORIGIN, "https://evil.example")]),
                Err(Refusal::Origin),
            ),
            (
                Method::PUT,
                headers(&[host.clone(), (header::ORIGIN, "null")]),
                Err(Refusal::Origin),
            ),
            (
                Method::POST,
                headers(&[(header::ORIGIN, "http://127.0.0.1:7240")]),
                Err(Refusal::Origin),
            ),
            (
                Method::POST,
                headers(&[
                    host.clone(),
                    (header::ORIGIN, "http://127.0.0.1:7240"),
                    (header::ORIGIN, "https://evil.example"),
                ]),
                Err(Refusal::Origin),
            ),
            (
                Method::GET,
                headers(&[host.clone(), (header::ORIGIN, "https://evil.example")]),
                Ok(()),
            ),
            (
                Method::OPTIONS,
                headers(&[host, (header::ORIGIN, "https://evil.example")]),
                Ok(()),
            ),
        ];
        for (method, map, want) in cases {
            assert_eq!(check_origin(&method, &map), want, "{method} {map:?}");
        }
    }

    #[test]
    fn check_content_type_table() {
        let json = (header::CONTENT_TYPE, "application/json");
        let cases = [
            (Method::POST, headers(std::slice::from_ref(&json)), Ok(())),
            (Method::DELETE, headers(std::slice::from_ref(&json)), Ok(())),
            (Method::POST, headers(&[]), Err(Refusal::ContentType)),
            (Method::DELETE, headers(&[]), Err(Refusal::ContentType)),
            (
                Method::POST,
                headers(&[(header::CONTENT_TYPE, "text/plain")]),
                Err(Refusal::ContentType),
            ),
            (
                Method::PUT,
                headers(&[json.clone(), (header::CONTENT_TYPE, "text/plain")]),
                Err(Refusal::ContentType),
            ),
            (Method::GET, headers(&[]), Ok(())),
            (
                Method::HEAD,
                headers(&[(header::CONTENT_TYPE, "text/plain")]),
                Ok(()),
            ),
        ];
        for (method, map, want) in cases {
            assert_eq!(check_content_type(&method, &map), want, "{method} {map:?}");
        }
    }

    #[test]
    fn refusals_map_to_status_and_code() {
        for (refusal, status, code) in [
            (Refusal::Host, StatusCode::FORBIDDEN, ErrorCode::Forbidden),
            (Refusal::Origin, StatusCode::FORBIDDEN, ErrorCode::Forbidden),
            (
                Refusal::ContentType,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ErrorCode::UnsupportedMediaType,
            ),
        ] {
            assert_eq!(refusal.status(), status, "{refusal:?}");
            assert_eq!(refusal.code(), code, "{refusal:?}");
            assert!(!refusal.message().is_empty(), "{refusal:?}");
        }
    }
}
