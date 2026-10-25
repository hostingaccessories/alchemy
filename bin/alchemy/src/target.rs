//! Which alchemyd the CLI talks to.
//!
//! `--daemon <url>` and `ALCHEMY_DAEMON` name the daemon by URL;
//! `--socket` and `ALCHEMY_SOCKET` name a local socket path and are
//! aliases for a `unix://` URL. Only `unix://` is accepted. A remote
//! daemon needs authentication (M13, #16), so every other scheme is
//! refused here, before anything is opened. With none of them, the CLI
//! looks for alchemyd at each of `alchemy_api::socket_candidates` in turn.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::{ClientError, InstalledService, Unreached};

/// Environment variable that names the daemon by URL, as `--daemon` does.
pub const DAEMON_ENV: &str = "ALCHEMY_DAEMON";

const DAEMON_FLAG: &str = "--daemon";
const SOCKET_FLAG: &str = "--socket";

/// Where alchemyd listens. Authenticated remote daemons join this in M13.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// A unix socket on this machine.
    Unix(PathBuf),
    /// Nothing named a daemon: the first of these default sockets that
    /// answers, as [`find`] picks it.
    Search(Vec<PathBuf>),
}

/// Everything target resolution chooses between. Empty environment
/// values count as unset, as they do for `alchemy_api::default_socket_path`.
#[derive(Debug, Default)]
pub(crate) struct Sources {
    pub(crate) daemon_flag: Option<String>,
    pub(crate) socket_flag: Option<PathBuf>,
    pub(crate) daemon_env: Option<OsString>,
    pub(crate) socket_env: Option<OsString>,
    pub(crate) default_sockets: Vec<PathBuf>,
}

impl Sources {
    /// The given flags plus this process's environment, looking for the
    /// daemon at `default_sockets` when none of them names one.
    pub(crate) fn from_env(
        daemon_flag: Option<String>,
        socket_flag: Option<PathBuf>,
        default_sockets: Vec<PathBuf>,
    ) -> Self {
        Self {
            daemon_flag,
            socket_flag,
            daemon_env: std::env::var_os(DAEMON_ENV),
            socket_env: std::env::var_os(alchemy_api::SOCKET_ENV),
            default_sockets,
        }
    }
}

/// Pick the daemon target: `--daemon`, then `--socket`, then
/// `ALCHEMY_DAEMON`, then `ALCHEMY_SOCKET`, then the default sockets.
///
/// The first source that is set decides, even when it is invalid; a bad
/// value never falls through to a lower-precedence one. The CLI rejects
/// `--daemon` together with `--socket` as a usage error, so `--daemon`
/// winning over `--socket` only matters to callers that build a `Cli`
/// by hand.
pub(crate) fn resolve(sources: Sources) -> Result<Target, ClientError> {
    if let Some(url) = sources.daemon_flag {
        return parse_url(DAEMON_FLAG, &url);
    }
    if let Some(path) = sources.socket_flag {
        return socket_path(SOCKET_FLAG, path);
    }
    if let Some(url) = non_empty(sources.daemon_env) {
        let url = url.into_string().map_err(|_| ClientError::InvalidTarget {
            origin: DAEMON_ENV,
            reason: "it is not valid UTF-8",
        })?;
        return parse_url(DAEMON_ENV, &url);
    }
    if let Some(path) = non_empty(sources.socket_env) {
        return Ok(Target::Unix(PathBuf::from(path)));
    }
    Ok(Target::Search(sources.default_sockets))
}

/// The first of `sockets` that `answers`, or every one tried and why it
/// didn't, with the autostart service `installed` finds, asked only then.
///
/// Only the default lookup searches. A socket the user named is used as
/// given, so its error comes from the request itself.
pub(crate) fn find(
    sockets: Vec<PathBuf>,
    mut answers: impl FnMut(&Path) -> std::io::Result<()>,
    installed: impl FnOnce() -> Option<InstalledService>,
) -> Result<PathBuf, ClientError> {
    let mut tried = Vec::with_capacity(sockets.len());
    for socket in sockets {
        match answers(&socket) {
            Ok(()) => return Ok(socket),
            Err(e) => tried.push(Unreached {
                socket,
                reason: e.to_string(),
            }),
        }
    }
    Err(ClientError::NoDaemon {
        tried,
        service: installed(),
    })
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|v| !v.is_empty())
}

fn socket_path(origin: &'static str, path: PathBuf) -> Result<Target, ClientError> {
    if path.as_os_str().is_empty() {
        return Err(ClientError::InvalidTarget {
            origin,
            reason: "the socket path is empty",
        });
    }
    Ok(Target::Unix(path))
}

/// Parse a daemon URL. `unix:///abs/path` becomes that path, taken
/// literally with no percent-decoding. Any other well-formed scheme is
/// refused as remote.
///
/// Errors never echo the URL: a remote one may carry credentials.
fn parse_url(origin: &'static str, url: &str) -> Result<Target, ClientError> {
    let invalid = |reason| ClientError::InvalidTarget { origin, reason };
    let (scheme, rest) = url.split_once("://").ok_or(invalid(
        "expected a URL such as unix:///path/to/alchemyd.sock",
    ))?;
    if !is_scheme(scheme) {
        return Err(invalid("the URL scheme is malformed"));
    }
    if !scheme.eq_ignore_ascii_case("unix") {
        return Err(ClientError::RemoteDaemon {
            origin,
            scheme: scheme.to_ascii_lowercase(),
        });
    }
    if !rest.starts_with('/') {
        return Err(invalid(
            "a unix:// URL needs an absolute path and no host, as in unix:///path/to/alchemyd.sock",
        ));
    }
    if rest.contains(['?', '#']) {
        return Err(invalid(
            "a unix:// URL takes a path only, with no query or fragment",
        ));
    }
    Ok(Target::Unix(PathBuf::from(rest)))
}

/// RFC 3986 `scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
fn is_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
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

    #[allow(clippy::unnecessary_wraps)]
    fn unix(path: &str) -> Result<Target, String> {
        Ok(Target::Unix(PathBuf::from(path)))
    }

    fn remote(origin: &str, scheme: &str) -> Result<Target, String> {
        Err(format!("remote {origin} {scheme}"))
    }

    fn invalid(origin: &str) -> Result<Target, String> {
        Err(format!("invalid {origin}"))
    }

    fn summarize(result: Result<Target, ClientError>) -> Result<Target, String> {
        result.map_err(|e| match e {
            ClientError::RemoteDaemon { origin, scheme } => format!("remote {origin} {scheme}"),
            ClientError::InvalidTarget { origin, .. } => format!("invalid {origin}"),
            other => panic!("unexpected error: {other}"),
        })
    }

    #[test]
    fn parses_daemon_urls() {
        let cases = [
            ("unix:///run/alchemyd.sock", unix("/run/alchemyd.sock")),
            ("UNIX:///run/alchemyd.sock", unix("/run/alchemyd.sock")),
            ("unix:///tmp/with space.sock", unix("/tmp/with space.sock")),
            ("unix:///tmp/a%20b.sock", unix("/tmp/a%20b.sock")),
            ("unix:///", unix("/")),
            ("https://x", remote("--daemon", "https")),
            ("HTTPS://user:secret@host:7240", remote("--daemon", "https")),
            ("http://127.0.0.1:7240", remote("--daemon", "http")),
            ("tcp://10.0.0.5:2375", remote("--daemon", "tcp")),
            ("ssh://user@host", remote("--daemon", "ssh")),
            ("git+ssh://host", remote("--daemon", "git+ssh")),
            ("unix://", invalid("--daemon")),
            ("unix://host/run/alchemyd.sock", invalid("--daemon")),
            ("unix://relative.sock", invalid("--daemon")),
            ("unix:///run/alchemyd.sock?x=1", invalid("--daemon")),
            ("unix:///run/alchemyd.sock#frag", invalid("--daemon")),
            ("unix:/run/alchemyd.sock", invalid("--daemon")),
            ("/run/alchemyd.sock", invalid("--daemon")),
            ("", invalid("--daemon")),
            ("://x", invalid("--daemon")),
            ("1http://x", invalid("--daemon")),
            ("ht tp://x", invalid("--daemon")),
        ];
        for (url, want) in cases {
            assert_eq!(summarize(parse_url(DAEMON_FLAG, url)), want, "{url}");
        }
    }

    fn sources(
        daemon_flag: Option<&str>,
        socket_flag: Option<&str>,
        daemon_env: Option<&str>,
        socket_env: Option<&str>,
    ) -> Sources {
        Sources {
            daemon_flag: daemon_flag.map(str::to_owned),
            socket_flag: socket_flag.map(PathBuf::from),
            daemon_env: daemon_env.map(OsString::from),
            socket_env: socket_env.map(OsString::from),
            default_sockets: vec![PathBuf::from("/default.sock")],
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn search(path: &str) -> Result<Target, String> {
        Ok(Target::Search(vec![PathBuf::from(path)]))
    }

    #[test]
    fn resolves_by_precedence() {
        let d = Some("unix:///flag-daemon.sock");
        let s = Some("/flag-socket.sock");
        let de = Some("unix:///env-daemon.sock");
        let se = Some("/env-socket.sock");
        let cases = [
            (sources(d, s, de, se), unix("/flag-daemon.sock")),
            (sources(None, s, de, se), unix("/flag-socket.sock")),
            (sources(None, None, de, se), unix("/env-daemon.sock")),
            (sources(None, None, None, se), unix("/env-socket.sock")),
            (sources(None, None, None, None), search("/default.sock")),
            (
                sources(None, Some("rel.sock"), None, None),
                unix("rel.sock"),
            ),
            (sources(None, None, Some(""), se), unix("/env-socket.sock")),
            (
                sources(None, None, Some(""), Some("")),
                search("/default.sock"),
            ),
            (sources(None, Some(""), de, se), invalid("--socket")),
            (sources(Some(""), s, de, se), invalid("--daemon")),
            (
                sources(Some("https://x"), s, de, se),
                remote("--daemon", "https"),
            ),
            (
                sources(None, None, Some("https://x"), se),
                remote("ALCHEMY_DAEMON", "https"),
            ),
            (
                sources(None, s, Some("https://x"), se),
                unix("/flag-socket.sock"),
            ),
            (
                sources(None, None, Some("bogus"), se),
                invalid("ALCHEMY_DAEMON"),
            ),
        ];
        for (i, (sources, want)) in cases.into_iter().enumerate() {
            assert_eq!(summarize(resolve(sources)), want, "case {i}");
        }
    }

    #[test]
    fn find_takes_the_first_socket_that_answers() {
        use std::io::{Error, ErrorKind};
        let answers = |live: &'static [&'static str]| {
            move |p: &Path| {
                if live.iter().any(|l| p == Path::new(l)) {
                    Ok(())
                } else {
                    Err(Error::from(ErrorKind::NotFound))
                }
            }
        };
        let both = || vec![PathBuf::from("/a.sock"), PathBuf::from("/b.sock")];
        let cases: [(&'static [&'static str], &str); 4] = [
            (&["/a.sock", "/b.sock"], "found /a.sock"),
            (&["/b.sock"], "found /b.sock"),
            (&["/a.sock"], "found /a.sock"),
            (&[], "tried /a.sock, /b.sock"),
        ];
        for (live, want) in cases {
            let got = match find(both(), answers(live), || None) {
                Ok(socket) => format!("found {}", socket.display()),
                Err(ClientError::NoDaemon { tried, .. }) => {
                    let tried: Vec<_> = tried
                        .iter()
                        .map(|t| t.socket.display().to_string())
                        .collect();
                    format!("tried {}", tried.join(", "))
                }
                Err(other) => panic!("unexpected error: {other}"),
            };
            assert_eq!(got, want, "{live:?}");
        }
    }

    #[test]
    fn find_asks_each_socket_once_in_order() {
        let mut asked = Vec::new();
        let err = find(
            vec![PathBuf::from("/a.sock"), PathBuf::from("/b.sock")],
            |p| {
                asked.push(p.to_owned());
                Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
            },
            || None,
        )
        .unwrap_err();
        assert_eq!(asked, [PathBuf::from("/a.sock"), PathBuf::from("/b.sock")]);
        assert_eq!(err.exit_code(), 69);
    }

    #[test]
    fn find_looks_for_a_service_only_when_no_socket_answers() {
        use alchemy_api::autostart::{AutostartMode, LogHint};
        let service = InstalledService {
            mode: AutostartMode::Login,
            logs: LogHint::Terminal,
        };
        let found = find(
            vec![PathBuf::from("/a.sock")],
            |_| Ok(()),
            || panic!("looked for a service with a daemon answering"),
        );
        assert_eq!(found.unwrap(), PathBuf::from("/a.sock"));
        let err = find(
            vec![PathBuf::from("/a.sock")],
            |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            || Some(service.clone()),
        )
        .unwrap_err();
        let ClientError::NoDaemon {
            service: Some(got), ..
        } = err
        else {
            panic!("{err:?}");
        };
        assert_eq!(got, service);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_non_utf8_daemon_env() {
        use std::os::unix::ffi::OsStringExt;
        let mut s = sources(None, None, None, Some("/env-socket.sock"));
        s.daemon_env = Some(OsString::from_vec(vec![0x75, 0xff]));
        assert_eq!(summarize(resolve(s)), invalid("ALCHEMY_DAEMON"));
    }

    #[test]
    fn errors_never_echo_the_url() {
        for url in ["https://user:secret@host", "htps//user:secret@host"] {
            let err = parse_url(DAEMON_FLAG, url).unwrap_err().to_string();
            assert!(!err.contains("secret"), "{err}");
        }
    }

    #[test]
    fn refusal_points_at_auth() {
        let err = parse_url(DAEMON_FLAG, "https://x").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        let msg = err.to_string();
        assert!(msg.contains("M13"), "{msg}");
        assert!(msg.contains("issues/16"), "{msg}");
        assert!(
            err.to_text()
                .contains("\nhint: forward the remote socket over SSH"),
            "{msg}"
        );
    }
}
