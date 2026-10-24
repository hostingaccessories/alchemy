//! Header hygiene for proxied requests and responses. Pure: every
//! function edits a [`HeaderMap`] in memory.
//!
//! The proxy is the edge, so what a client says about forwarding is
//! never trusted: every incoming `X-Forwarded-*`, `Forwarded`, and
//! `X-Real-IP` header is dropped and the proxy's own are set. Hop-by-hop
//! headers (RFC 9110 section 7.6.1) are dropped both ways, and so is
//! every Alchemy session cookie, which browsers send to every port on a
//! host.

use std::net::IpAddr;

use hyper::HeaderMap;
use hyper::header::{self, HeaderName, HeaderValue};

use crate::auth::SESSION_COOKIE_PREFIX;

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_REAL_IP: HeaderName = HeaderName::from_static("x-real-ip");

/// Whether `name` is hop-by-hop whatever `Connection` says.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// The header names listed in every `Connection` header, lowercased.
fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

/// The protocol a request asks to switch to: its `Upgrade` header, when
/// `Connection` also names `upgrade`.
pub fn upgrade_protocol(headers: &HeaderMap) -> Option<HeaderValue> {
    connection_tokens(headers)
        .iter()
        .any(|token| token == "upgrade")
        .then(|| headers.get(header::UPGRADE).cloned())
        .flatten()
}

/// Drop the hop-by-hop headers and every header `Connection` names.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed = connection_tokens(headers);
    let fixed: Vec<HeaderName> = headers
        .keys()
        .filter(|n| is_hop_by_hop(n))
        .cloned()
        .collect();
    for name in fixed {
        headers.remove(name);
    }
    for token in listed {
        if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
            headers.remove(name);
        }
    }
}

/// Whether a `name=value` cookie pair, or the start of a `Set-Cookie`
/// value, is an Alchemy session cookie. The prefix is matched ignoring
/// case, which is stricter than the daemon's own exact match.
fn is_session_cookie(pair: &[u8]) -> bool {
    let name = pair
        .split(|&b| b == b'=' || b == b';')
        .next()
        .unwrap_or_default()
        .trim_ascii();
    name.get(..SESSION_COOKIE_PREFIX.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(SESSION_COOKIE_PREFIX.as_bytes()))
}

/// Drop every Alchemy session cookie from the `Cookie` headers, joining
/// what is left into one header, or none when nothing is.
pub fn strip_session_cookies(headers: &mut HeaderMap) {
    let values: Vec<HeaderValue> = headers.get_all(header::COOKIE).iter().cloned().collect();
    if values.is_empty() {
        return;
    }
    headers.remove(header::COOKIE);
    let mut kept: Vec<u8> = Vec::new();
    for pair in values
        .iter()
        .flat_map(|value| value.as_bytes().split(|&b| b == b';'))
        .map(<[u8]>::trim_ascii)
        .filter(|pair| !pair.is_empty() && !is_session_cookie(pair))
    {
        if !kept.is_empty() {
            kept.extend_from_slice(b"; ");
        }
        kept.extend_from_slice(pair);
    }
    if let Ok(value) = HeaderValue::from_bytes(&kept)
        && !kept.is_empty()
    {
        headers.insert(header::COOKIE, value);
    }
}

/// Drop every `Set-Cookie` that would set an Alchemy session cookie, so
/// an app can't plant or clobber the daemon's session on a shared host.
pub fn strip_session_set_cookies(headers: &mut HeaderMap) {
    let values: Vec<HeaderValue> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter(|value| !is_session_cookie(value.as_bytes()))
        .cloned()
        .collect();
    headers.remove(header::SET_COOKIE);
    for value in values {
        headers.append(header::SET_COOKIE, value);
    }
}

/// Who a request came from, for the `X-Forwarded-*` headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forwarded {
    /// The client's address.
    pub client: IpAddr,
    /// The `Host` the client asked for, if it sent one.
    pub host: Option<HeaderValue>,
}

/// Make a client's request headers fit to forward to an app.
///
/// Drops hop-by-hop headers, `Expect` (already answered), session
/// cookies, and any forwarding headers the client sent, then sets
/// `X-Forwarded-For`, `-Host`, and `-Proto`, keeps `Host`, and restores
/// `Upgrade` when the request switches protocols to `upgrade`.
pub fn forward_request(headers: &mut HeaderMap, from: &Forwarded, upgrade: Option<&HeaderValue>) {
    strip_hop_by_hop(headers);
    strip_session_cookies(headers);
    let untrusted: Vec<HeaderName> = headers
        .keys()
        .filter(|name| {
            name.as_str().starts_with("x-forwarded-")
                || *name == header::FORWARDED
                || *name == X_REAL_IP
                || *name == header::EXPECT
        })
        .cloned()
        .collect();
    for name in untrusted {
        headers.remove(name);
    }
    headers.remove(header::HOST);
    if let Some(host) = &from.host {
        headers.insert(header::HOST, host.clone());
        headers.insert(X_FORWARDED_HOST, host.clone());
    }
    if let Ok(client) = HeaderValue::from_str(&from.client.to_canonical().to_string()) {
        headers.insert(X_FORWARDED_FOR, client);
    }
    headers.insert(X_FORWARDED_PROTO, HeaderValue::from_static("http"));
    if let Some(protocol) = upgrade {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(header::UPGRADE, protocol.clone());
    }
}

/// Make an app's response headers fit to return to the client.
///
/// Drops hop-by-hop headers and session `Set-Cookie`s. When `switching`
/// (a `101 Switching Protocols`), keeps the app's `Upgrade` and sets
/// `Connection: upgrade`.
pub fn forward_response(headers: &mut HeaderMap, switching: bool) {
    let upgrade = headers.get(header::UPGRADE).cloned().filter(|_| switching);
    strip_hop_by_hop(headers);
    strip_session_set_cookies(headers);
    if let Some(protocol) = upgrade {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(header::UPGRADE, protocol);
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    type Pairs = &'static [(&'static str, &'static str)];

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    fn all(headers: &HeaderMap, name: &str) -> Vec<String> {
        headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn session_cookies_table() {
        let cases: &[(&[&str], Option<&str>)] = &[
            (&["a=1"], Some("a=1")),
            (&["alchemy_session_7240=s"], None),
            (&["a=1; alchemy_session_7240=s; b=2"], Some("a=1; b=2")),
            (&["ALCHEMY_SESSION_7240=s;b=2"], Some("b=2")),
            (
                &["alchemy_session_1=x", "a=1", "alchemy_session_2=y"],
                Some("a=1"),
            ),
            (&["a=1", "b=2"], Some("a=1; b=2")),
            (&[" alchemy_session_7240 = s ;  c=3 "], Some("c=3")),
            (
                &["alchemy_sessionx=1; alchemy=2"],
                Some("alchemy_sessionx=1; alchemy=2"),
            ),
            (
                &["xalchemy_session_7240=s"],
                Some("xalchemy_session_7240=s"),
            ),
            (&[";;"], None),
        ];
        for (cookies, want) in cases {
            let pairs: Vec<(&str, &str)> = cookies.iter().map(|c| ("cookie", *c)).collect();
            let mut headers = map(&pairs);
            strip_session_cookies(&mut headers);
            let want: Vec<String> = want.iter().map(|w| (*w).to_owned()).collect();
            assert_eq!(all(&headers, "cookie"), want, "{cookies:?}");
        }
    }

    #[test]
    fn session_set_cookies_table() {
        let cases: &[(&[&str], &[&str])] = &[
            (&["a=1; Path=/"], &["a=1; Path=/"]),
            (&["alchemy_session_7240=s; Path=/api"], &[]),
            (
                &["a=1", "Alchemy_Session_7240=s", "b=2; HttpOnly"],
                &["a=1", "b=2; HttpOnly"],
            ),
            (&["alchemy=1"], &["alchemy=1"]),
        ];
        for (set, want) in cases {
            let pairs: Vec<(&str, &str)> = set.iter().map(|c| ("set-cookie", *c)).collect();
            let mut headers = map(&pairs);
            strip_session_set_cookies(&mut headers);
            assert_eq!(all(&headers, "set-cookie"), *want, "{set:?}");
        }
    }

    #[test]
    fn hop_by_hop_table() {
        let cases: &[(Pairs, &[&str])] = &[
            (
                &[
                    ("connection", "keep-alive"),
                    ("keep-alive", "timeout=5"),
                    ("transfer-encoding", "chunked"),
                    ("te", "trailers"),
                    ("trailer", "x"),
                    ("upgrade", "websocket"),
                    ("proxy-authorization", "Basic x"),
                    ("proxy-connection", "keep-alive"),
                    ("accept", "*/*"),
                ],
                &["accept"],
            ),
            (
                &[
                    ("connection", "X-Secret, close"),
                    ("x-secret", "1"),
                    ("content-length", "3"),
                ],
                &["content-length"],
            ),
            (&[("accept", "*/*")], &["accept"]),
        ];
        for (pairs, want) in cases {
            let mut headers = map(pairs);
            strip_hop_by_hop(&mut headers);
            let mut names: Vec<&str> = headers.keys().map(HeaderName::as_str).collect();
            names.sort_unstable();
            assert_eq!(names, *want, "{pairs:?}");
        }
    }

    #[test]
    fn upgrade_protocol_table() {
        let cases: &[(Pairs, Option<&str>)] = &[
            (
                &[("connection", "Upgrade"), ("upgrade", "websocket")],
                Some("websocket"),
            ),
            (
                &[("connection", "keep-alive, upgrade"), ("upgrade", "h2c")],
                Some("h2c"),
            ),
            (&[("upgrade", "websocket")], None),
            (&[("connection", "upgrade")], None),
            (&[], None),
        ];
        for (pairs, want) in cases {
            let got = upgrade_protocol(&map(pairs));
            assert_eq!(
                got.as_ref().map(|v| v.to_str().unwrap()),
                *want,
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn forward_request_sets_its_own_forwarding_headers() {
        let mut headers = map(&[
            ("host", "wiki.localhost"),
            ("x-forwarded-for", "6.6.6.6"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-ssl", "on"),
            ("forwarded", "for=6.6.6.6"),
            ("x-real-ip", "6.6.6.6"),
            ("expect", "100-continue"),
            ("cookie", "alchemy_session_7240=s; theme=dark"),
            ("connection", "keep-alive"),
            ("accept", "text/html"),
        ]);
        let from = Forwarded {
            client: IpAddr::V6(Ipv4Addr::new(192, 168, 1, 9).to_ipv6_mapped()),
            host: Some(HeaderValue::from_static("wiki.localhost")),
        };
        forward_request(&mut headers, &from, None);
        let mut got: Vec<(String, String)> = headers
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_str().unwrap().to_owned()))
            .collect();
        got.sort();
        let want: Vec<(String, String)> = [
            ("accept", "text/html"),
            ("cookie", "theme=dark"),
            ("host", "wiki.localhost"),
            ("x-forwarded-for", "192.168.1.9"),
            ("x-forwarded-host", "wiki.localhost"),
            ("x-forwarded-proto", "http"),
        ]
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn forward_request_keeps_an_upgrade_and_cannot_be_told_to_drop_host() {
        let mut headers = map(&[
            ("host", "127.0.0.1:8100"),
            ("connection", "Upgrade, Host"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "abc"),
        ]);
        let upgrade = upgrade_protocol(&headers);
        let from = Forwarded {
            client: IpAddr::V6(Ipv6Addr::LOCALHOST),
            host: Some(HeaderValue::from_static("127.0.0.1:8100")),
        };
        forward_request(&mut headers, &from, upgrade.as_ref());
        assert_eq!(all(&headers, "connection"), ["upgrade"]);
        assert_eq!(all(&headers, "upgrade"), ["websocket"]);
        assert_eq!(all(&headers, "host"), ["127.0.0.1:8100"]);
        assert_eq!(all(&headers, "sec-websocket-key"), ["abc"]);
        assert_eq!(all(&headers, "x-forwarded-for"), ["::1"]);
    }

    #[test]
    fn forward_request_without_host_sets_no_forwarded_host() {
        let mut headers = map(&[("x-forwarded-host", "evil.example")]);
        let from = Forwarded {
            client: IpAddr::V4(Ipv4Addr::LOCALHOST),
            host: None,
        };
        forward_request(&mut headers, &from, None);
        assert!(headers.get("host").is_none());
        assert!(headers.get("x-forwarded-host").is_none());
    }

    #[test]
    fn forward_response_table() {
        let cases: &[(bool, Pairs, Pairs)] = &[
            (
                true,
                &[
                    ("connection", "Upgrade"),
                    ("upgrade", "websocket"),
                    ("sec-websocket-accept", "x"),
                ],
                &[
                    ("connection", "upgrade"),
                    ("sec-websocket-accept", "x"),
                    ("upgrade", "websocket"),
                ],
            ),
            (
                false,
                &[
                    ("connection", "close"),
                    ("upgrade", "websocket"),
                    ("set-cookie", "alchemy_session_7240=x"),
                    ("set-cookie", "a=1"),
                    ("content-type", "text/plain"),
                ],
                &[("content-type", "text/plain"), ("set-cookie", "a=1")],
            ),
        ];
        for (switching, pairs, want) in cases {
            let mut headers = map(pairs);
            forward_response(&mut headers, *switching);
            let mut got: Vec<(String, String)> = headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_str().unwrap().to_owned()))
                .collect();
            got.sort();
            let want: Vec<(String, String)> = want
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect();
            assert_eq!(got, want, "{pairs:?}");
        }
    }
}
