//! Sign-in rules for the TCP listener.
//!
//! The [`crate::guard`] checks keep other web pages away from the port,
//! but not other processes or other users on this machine: anything that
//! can connect to loopback could otherwise create an app, which is root
//! by another name, or read an app's `.env`. So every request under `/api`
//! on the port, except the few in [`is_public`], must carry a credential:
//!
//! - the daemon's token, from the `0o600` token file, as
//!   `Authorization: Bearer TOKEN` ([`bearer`]), or
//! - a session cookie ([`cookie_name`], [`set_cookie`]) that the browser
//!   got by redeeming a one-time [`LaunchCodes`] code at `/login`.
//!
//! The cookie holds a random session id, never the token, and the daemon
//! keeps only digests of ids and codes ([`Sessions`]), so neither is
//! compared or stored in the clear. Browsers do not keep cookies apart by
//! port: one set by `127.0.0.1:7240` goes to every port on `127.0.0.1`
//! whose path matches. So the name carries the daemon's port, to keep two
//! daemons apart, the path is `/api` alone, and a session dies with the
//! daemon.
//!
//! Everything here is pure. The clock is passed in, and secrets are made
//! by the caller; [`crate::session`] holds the live state.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use alchemy_api::session::SessionAuth;
use axum::http::{HeaderMap, Method, header};
use sha2::{Digest as _, Sha256};

/// Random bytes in a token, session id, or launch code: 256 bits.
pub const SECRET_BYTES: usize = 32;

/// How long a launch code stays valid.
pub const LAUNCH_CODE_TTL: Duration = Duration::from_mins(1);

/// Launch codes kept at once. Issuing one past this drops the oldest.
pub const MAX_LAUNCH_CODES: usize = 16;

/// Browser sessions kept at once. Signing in past this ends the oldest.
pub const MAX_SESSIONS: usize = 64;

/// The path of the sign-in route on the TCP listener.
pub const LOGIN_PATH: &str = "/login";

/// The path, under `/api`, of the socket-only route that issues launch
/// codes.
pub const LAUNCH_PATH: &str = "/session/launch";

/// A SHA-256 digest of a secret, which is what [`LaunchCodes`] and
/// [`Sessions`] keep.
type Digest = [u8; 32];

fn digest(secret: &str) -> Digest {
    Sha256::digest(secret.as_bytes()).into()
}

/// Whether a request for `path` with `method` may reach the TCP listener's
/// routes without a credential.
///
/// Public: everything outside `/api`, which is the web UI's static files
/// and `/login`, and `GET` or `HEAD` of `/api/health` and `/api/version`,
/// so the UI and the shell can tell alchemyd is running before signing
/// in. Neither says anything about the apps.
pub fn is_public(method: &Method, path: &str) -> bool {
    if !crate::guard::is_api_path(path) {
        return true;
    }
    matches!(*method, Method::GET | Method::HEAD) && matches!(path, "/api/health" | "/api/version")
}

/// Whether `given` equals `expected`, taking the same time whatever bytes
/// differ. Only the length can be learnt from timing, and a token's
/// length is not secret.
pub fn secrets_match(given: &str, expected: &str) -> bool {
    let (given, expected) = (given.as_bytes(), expected.as_bytes());
    if given.len() != expected.len() {
        return false;
    }
    let diff = given
        .iter()
        .zip(expected)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    std::hint::black_box(diff) == 0
}

/// The bearer token in the request's one `Authorization` header, if it
/// has exactly one and its scheme, in any case, is `Bearer`.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return None;
    };
    let value = value.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// Every value of the cookie `name` in the request's `Cookie` headers, in
/// order. A browser may send more than one, set for different paths.
pub fn cookies<'a>(headers: &'a HeaderMap, name: &'a str) -> impl Iterator<Item = &'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(move |pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then_some(value.trim())
        })
}

/// What every session cookie's name starts with. The proxy strips
/// cookies with this prefix before forwarding a request to an app.
pub const SESSION_COOKIE_PREFIX: &str = "alchemy_session_";

/// The session cookie's name for a daemon on `port`. Browsers send a
/// cookie to every port on its host, so two daemons on one machine each
/// need their own.
pub fn cookie_name(port: u16) -> String {
    format!("{SESSION_COOKIE_PREFIX}{port}")
}

/// The `Set-Cookie` value for session `id`.
///
/// `HttpOnly` keeps it from scripts, and `SameSite=Strict` from requests
/// other sites start. It has no `Secure`, since the listener is plain
/// `http://` on loopback and a browser would refuse it, and no `Max-Age`,
/// so it ends with the browser session. `Path=/api` keeps it off the web
/// UI's files, which need no credential.
pub fn set_cookie(name: &str, id: &str) -> String {
    format!("{name}={id}; HttpOnly; SameSite=Strict; Path=/api")
}

/// Lowercase hex of `bytes`, which is how secrets are written: safe in a
/// header, a cookie, a URL, and a file.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Whether `secret` looks like one [`hex`] wrote from [`SECRET_BYTES`]
/// bytes, so anything else is refused before it is digested or compared.
pub fn is_secret(secret: &str) -> bool {
    secret.len() == SECRET_BYTES * 2
        && secret
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The `http://` origin a browser on this machine reaches a daemon
/// listening on `listen` at. An unspecified address is reached on
/// loopback.
pub fn launch_origin(listen: SocketAddr) -> String {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    format!("http://{}", SocketAddr::new(ip, listen.port()))
}

/// The sign-in link at `origin` for launch `code`.
pub fn login_url(origin: &str, code: &str) -> String {
    format!("{origin}{LOGIN_PATH}?code={code}")
}

/// How a request on the TCP listener may be let in, given the `token` and
/// the browser `sessions`, whose cookie is `cookie`. `None` means it
/// carried no valid credential.
///
/// A bearer header that is present but wrong is refused outright, even if
/// the request also has a valid cookie, so a client never silently signs
/// in as something other than it asked.
pub fn authenticate(
    headers: &HeaderMap,
    token: &str,
    cookie: &str,
    sessions: &Sessions,
) -> Option<SessionAuth> {
    if headers.contains_key(header::AUTHORIZATION) {
        return bearer(headers)
            .is_some_and(|given| secrets_match(given, token))
            .then_some(SessionAuth::Token);
    }
    cookies(headers, cookie)
        .any(|id| sessions.contains(id))
        .then_some(SessionAuth::Cookie)
}

/// One-time launch codes, each valid until it is redeemed or
/// [`LAUNCH_CODE_TTL`] passes.
#[derive(Debug, Default)]
pub struct LaunchCodes {
    expiry: HashMap<Digest, Instant>,
    order: VecDeque<Digest>,
}

impl LaunchCodes {
    /// No codes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Accept `code` from `now` until [`LAUNCH_CODE_TTL`] later. Expired
    /// codes are dropped, and past [`MAX_LAUNCH_CODES`] so is the oldest.
    pub fn issue(&mut self, code: &str, now: Instant) {
        self.prune(now);
        while self.order.len() >= MAX_LAUNCH_CODES {
            if let Some(oldest) = self.order.pop_front() {
                self.expiry.remove(&oldest);
            }
        }
        let key = digest(code);
        self.expiry.insert(key, now + LAUNCH_CODE_TTL);
        self.order.push_back(key);
    }

    /// Use up `code` at `now`: whether it was issued and has not expired
    /// or been redeemed already. It never works again either way.
    pub fn redeem(&mut self, code: &str, now: Instant) -> bool {
        self.prune(now);
        if !is_secret(code) {
            return false;
        }
        let key = digest(code);
        self.order.retain(|k| *k != key);
        self.expiry.remove(&key).is_some_and(|until| now < until)
    }

    /// How many codes are still held.
    pub fn len(&self) -> usize {
        self.expiry.len()
    }

    /// Whether no codes are held.
    pub fn is_empty(&self) -> bool {
        self.expiry.is_empty()
    }

    fn prune(&mut self, now: Instant) {
        self.expiry.retain(|_, until| now < *until);
        let expiry = &self.expiry;
        self.order.retain(|key| expiry.contains_key(key));
    }
}

/// Browser sessions signed in with a launch code, kept until the daemon
/// stops or [`MAX_SESSIONS`] newer ones push them out.
#[derive(Debug, Default)]
pub struct Sessions {
    order: VecDeque<Digest>,
}

impl Sessions {
    /// No sessions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start session `id`, ending the oldest past [`MAX_SESSIONS`].
    pub fn insert(&mut self, id: &str) {
        while self.order.len() >= MAX_SESSIONS {
            self.order.pop_front();
        }
        self.order.push_back(digest(id));
    }

    /// Whether `id` is a live session.
    pub fn contains(&self, id: &str) -> bool {
        is_secret(id) && self.order.contains(&digest(id))
    }

    /// How many sessions are live.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether no session is live.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
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
    use axum::http::{HeaderName, HeaderValue};

    fn secret(byte: u8) -> String {
        hex(&[byte; SECRET_BYTES])
    }

    fn headers(pairs: &[(HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn public_routes_table() {
        let cases = [
            (Method::GET, "/api/health", true),
            (Method::HEAD, "/api/health", true),
            (Method::GET, "/api/version", true),
            (Method::POST, "/api/health", false),
            (Method::GET, "/api/health/", false),
            (Method::GET, "/api/engine", false),
            (Method::GET, "/api/apps", false),
            (Method::GET, "/api/session", false),
            (Method::POST, "/api/session/launch", false),
            (Method::GET, "/api/events", false),
            (Method::GET, "/api", false),
            (Method::OPTIONS, "/api/apps", false),
            (Method::GET, "/", true),
            (Method::GET, "/login", true),
            (Method::GET, "/assets/index.js", true),
            (Method::GET, "/apps/blog", true),
            (Method::GET, "/apis", true),
        ];
        for (method, path, want) in cases {
            assert_eq!(is_public(&method, path), want, "{method} {path}");
        }
    }

    #[test]
    fn secrets_match_table() {
        let cases = [
            ("abc", "abc", true),
            ("abd", "abc", false),
            ("ab", "abc", false),
            ("abcd", "abc", false),
            ("", "abc", false),
            ("", "", true),
            ("ABC", "abc", false),
        ];
        for (given, expected, want) in cases {
            assert_eq!(secrets_match(given, expected), want, "{given:?}");
        }
    }

    #[test]
    fn bearer_table() {
        let auth = |v| (header::AUTHORIZATION, v);
        let cases = [
            (headers(&[auth("Bearer abc")]), Some("abc")),
            (headers(&[auth("bearer abc")]), Some("abc")),
            (headers(&[auth("BEARER  abc ")]), Some("abc")),
            (headers(&[auth("Bearer ")]), None),
            (headers(&[auth("Bearer")]), None),
            (headers(&[auth("Basic YWxhZGRpbjpvcGVu")]), None),
            (headers(&[auth("Bearerabc")]), None),
            (headers(&[auth("Bearer a"), auth("Bearer b")]), None),
            (headers(&[]), None),
        ];
        for (map, want) in cases {
            assert_eq!(bearer(&map), want, "{map:?}");
        }
        let mut opaque = HeaderMap::new();
        opaque.insert(
            header::AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xff").unwrap(),
        );
        assert_eq!(bearer(&opaque), None);
    }

    #[test]
    fn cookies_table() {
        let cookie = |v| (header::COOKIE, v);
        let name = "alchemy_session_7240";
        let cases: [(HeaderMap, &[&str]); 7] = [
            (headers(&[cookie("alchemy_session_7240=a")]), &["a"]),
            (
                headers(&[cookie("theme=dark; alchemy_session_7240=a; x=y")]),
                &["a"],
            ),
            (
                headers(&[cookie("alchemy_session_7240=a; alchemy_session_7240=b")]),
                &["a", "b"],
            ),
            (
                headers(&[
                    cookie("alchemy_session_7240=a"),
                    cookie("alchemy_session_7240=b"),
                ]),
                &["a", "b"],
            ),
            (headers(&[cookie("alchemy_session_17240=a")]), &[]),
            (headers(&[cookie("xalchemy_session_7240=a; garbage")]), &[]),
            (headers(&[]), &[]),
        ];
        for (map, want) in cases {
            assert_eq!(cookies(&map, name).collect::<Vec<_>>(), want, "{map:?}");
        }
    }

    #[test]
    fn cookie_is_scoped_to_the_port_and_the_api() {
        assert_eq!(cookie_name(7240), "alchemy_session_7240");
        assert_eq!(
            set_cookie("alchemy_session_7240", "abc"),
            "alchemy_session_7240=abc; HttpOnly; SameSite=Strict; Path=/api"
        );
    }

    #[test]
    fn hex_and_is_secret() {
        assert_eq!(hex(&[0, 1, 0xab, 0xff]), "0001abff");
        assert!(is_secret(&secret(0xab)));
        let cases = [
            String::new(),
            "ab".repeat(31),
            "ab".repeat(33),
            "AB".repeat(32),
            "zz".repeat(32),
        ];
        for case in cases {
            assert!(!is_secret(&case), "{case:?}");
        }
    }

    #[test]
    fn launch_origin_table() {
        let cases = [
            ("127.0.0.1:7240", "http://127.0.0.1:7240"),
            ("0.0.0.0:7240", "http://127.0.0.1:7240"),
            ("192.168.1.5:80", "http://192.168.1.5:80"),
            ("[::1]:7240", "http://[::1]:7240"),
            ("[::]:7240", "http://[::1]:7240"),
        ];
        for (listen, want) in cases {
            assert_eq!(launch_origin(listen.parse().unwrap()), want, "{listen}");
        }
        assert_eq!(
            login_url("http://127.0.0.1:7240", "c0de"),
            "http://127.0.0.1:7240/login?code=c0de"
        );
    }

    #[test]
    fn authenticate_table() {
        let token = secret(1);
        let session = secret(2);
        let mut sessions = Sessions::new();
        sessions.insert(&session);
        let name = "alchemy_session_7240";
        let bearer = |t: &str| (header::AUTHORIZATION, format!("Bearer {t}"));
        let cookie = |id: &str| (header::COOKIE, format!("{name}={id}"));
        let other = secret(3);
        let cases = [
            ("token", vec![bearer(&token)], Some(SessionAuth::Token)),
            ("wrong token", vec![bearer(&other)], None),
            ("session as token", vec![bearer(&session)], None),
            ("cookie", vec![cookie(&session)], Some(SessionAuth::Cookie)),
            ("unknown cookie", vec![cookie(&other)], None),
            ("token as cookie", vec![cookie(&token)], None),
            (
                "stale then live cookie",
                vec![(header::COOKIE, format!("{name}={other}; {name}={session}"))],
                Some(SessionAuth::Cookie),
            ),
            (
                "wrong token beside a cookie",
                vec![bearer(&other), cookie(&session)],
                None,
            ),
            (
                "malformed authorization beside a cookie",
                vec![
                    (header::AUTHORIZATION, "Basic x".to_owned()),
                    cookie(&session),
                ],
                None,
            ),
            ("nothing", vec![], None),
        ];
        for (name_, pairs, want) in cases {
            let mut map = HeaderMap::new();
            for (header, value) in pairs {
                map.append(header, HeaderValue::from_str(&value).unwrap());
            }
            assert_eq!(authenticate(&map, &token, name, &sessions), want, "{name_}");
        }
    }

    #[test]
    fn launch_codes_work_once_and_expire() {
        let t0 = Instant::now();
        let code = secret(7);
        let cases = [
            ("at once", Duration::ZERO, true),
            (
                "just before expiry",
                LAUNCH_CODE_TTL
                    .checked_sub(Duration::from_millis(1))
                    .unwrap(),
                true,
            ),
            ("at expiry", LAUNCH_CODE_TTL, false),
            ("long after", LAUNCH_CODE_TTL * 10, false),
        ];
        for (name, after, want) in cases {
            let mut codes = LaunchCodes::new();
            codes.issue(&code, t0);
            assert_eq!(codes.redeem(&code, t0 + after), want, "{name}");
            assert!(!codes.redeem(&code, t0 + after), "{name}: used twice");
            assert!(codes.is_empty(), "{name}");
        }
    }

    #[test]
    fn launch_codes_refuse_what_was_never_issued() {
        let t0 = Instant::now();
        let mut codes = LaunchCodes::new();
        codes.issue(&secret(0xab), t0);
        for code in [
            secret(2),
            String::new(),
            "not hex".to_owned(),
            secret(0xab).to_uppercase(),
        ] {
            assert!(!codes.redeem(&code, t0), "{code:?}");
        }
        assert!(codes.redeem(&secret(0xab), t0));
    }

    #[test]
    fn launch_codes_are_bounded() {
        let t0 = Instant::now();
        let mut codes = LaunchCodes::new();
        let all: Vec<String> = (0..=u8::try_from(MAX_LAUNCH_CODES).unwrap())
            .map(secret)
            .collect();
        for code in &all {
            codes.issue(code, t0);
        }
        assert_eq!(codes.len(), MAX_LAUNCH_CODES);
        assert!(!codes.redeem(&all[0], t0), "the oldest was dropped");
        assert!(codes.redeem(&all[MAX_LAUNCH_CODES], t0));

        let mut codes = LaunchCodes::new();
        codes.issue(&secret(1), t0);
        codes.issue(&secret(2), t0 + LAUNCH_CODE_TTL);
        assert_eq!(codes.len(), 1, "issuing drops expired codes");
    }

    #[test]
    fn sessions_are_bounded() {
        let mut sessions = Sessions::new();
        let all: Vec<String> = (0..=u8::try_from(MAX_SESSIONS).unwrap())
            .map(secret)
            .collect();
        for id in &all {
            sessions.insert(id);
        }
        assert_eq!(sessions.len(), MAX_SESSIONS);
        assert!(!sessions.contains(&all[0]));
        assert!(sessions.contains(&all[1]));
        assert!(sessions.contains(&all[MAX_SESSIONS]));
        assert!(!sessions.contains("not a secret"));
    }
}
