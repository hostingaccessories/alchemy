//! The environment a `docker compose` child runs with.
//!
//! Compose interpolates `${VAR}` in a compose file from its own environment,
//! so anything the daemon inherited could otherwise leak into a stack. The
//! child gets an empty environment plus the allowlist below, decided by the
//! pure [`child_env`].
//!
//! Per-stack variables come from the stack's own `.env`: Compose reads it
//! from the project directory itself, and its values take part in
//! interpolation alongside these. Alchemy passes no other per-stack values.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

/// Variables passed through from the daemon's environment by exact name.
///
/// Each one is needed by the `docker` CLI, Compose, or a credential helper:
///
/// * `PATH`: the CLI finds `docker-credential-*` helpers on it.
/// * `HOME`: the CLI reads `~/.docker/config.json` and its plugins there.
/// * `USER`, `LOGNAME`: who the user is, for helpers and `${USER}`.
/// * `LANG`, `TZ`: the locale and time zone of messages and timestamps.
/// * `TMPDIR`: where the CLI and `BuildKit` put temporary files; per-user
///   on macOS.
/// * `DOCKER_CONFIG`: a config directory other than `~/.docker`, holding
///   auth and the `credsStore` setting.
/// * `DBUS_SESSION_BUS_ADDRESS`: `docker-credential-secretservice` reaches
///   the desktop keyring over the session bus on Linux.
///
/// Left out on purpose: the other `DOCKER_*` variables, because the daemon
/// chooses the engine itself (see [`child_env`]); every `COMPOSE_*`
/// variable, because each would override the project's files, name, or
/// profiles; proxy settings and `SSH_AUTH_SOCK`, which can carry
/// credentials into builds; and everything else.
pub const ALLOWED_ENV: [&str; 9] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TZ",
    "TMPDIR",
    "DOCKER_CONFIG",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Prefixes of variables passed through from the daemon's environment.
///
/// `LC_` covers the locale categories alongside `LANG`. `XDG_` covers the
/// base directories (`XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR`, ...), which
/// rootless Docker, credential helpers, and D-Bus fall back on.
pub const ALLOWED_ENV_PREFIXES: [&str; 2] = ["LC_", "XDG_"];

/// Variables the child always gets, whatever the daemon's environment.
///
/// No colours or terminal escapes in the output, and no "What's next"
/// hints from the CLI, so each line is plain text or one JSON object.
pub const FIXED_ENV: [(&str, &str); 2] = [("COMPOSE_ANSI", "never"), ("DOCKER_CLI_HINTS", "false")];

/// Whether `name` is passed through from the daemon's environment.
pub fn is_allowed(name: &OsStr) -> bool {
    let name = name.as_encoded_bytes();
    ALLOWED_ENV.iter().any(|allowed| name == allowed.as_bytes())
        || ALLOWED_ENV_PREFIXES
            .iter()
            .any(|prefix| name.len() > prefix.len() && name.starts_with(prefix.as_bytes()))
}

/// The child's whole environment, given the daemon's as `parent`.
///
/// The allowed variables from `parent`, then [`FIXED_ENV`], then
/// `DOCKER_HOST` set to `docker_host`. A later duplicate in `parent` wins,
/// as it would in a real environment.
pub fn child_env<I, K, V>(
    parent: I,
    docker_host: impl Into<OsString>,
) -> BTreeMap<OsString, OsString>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    let mut env: BTreeMap<OsString, OsString> = parent
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .filter(|(name, _)| is_allowed(name))
        .collect();
    env.extend(
        FIXED_ENV
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value))),
    );
    env.insert("DOCKER_HOST".into(), docker_host.into());
    env
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

    type Pairs = &'static [(&'static str, &'static str)];

    const HOST: &str = "unix:///run/user/1000/docker.sock";

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn fixed(extra: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        let mut want = env(&[
            ("COMPOSE_ANSI", "never"),
            ("DOCKER_CLI_HINTS", "false"),
            ("DOCKER_HOST", HOST),
        ]);
        want.extend(env(extra));
        want
    }

    #[test]
    fn passes_only_the_allowlist() {
        let cases: [(Pairs, Pairs); 9] = [
            (&[], &[]),
            (
                &[
                    ("PATH", "/usr/bin:/bin"),
                    ("HOME", "/home/me"),
                    ("USER", "me"),
                    ("LOGNAME", "me"),
                    ("LANG", "en_US.UTF-8"),
                    ("TZ", "UTC"),
                    ("TMPDIR", "/tmp/me"),
                    ("DOCKER_CONFIG", "/home/me/.config/docker"),
                    ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
                ],
                &[
                    ("PATH", "/usr/bin:/bin"),
                    ("HOME", "/home/me"),
                    ("USER", "me"),
                    ("LOGNAME", "me"),
                    ("LANG", "en_US.UTF-8"),
                    ("TZ", "UTC"),
                    ("TMPDIR", "/tmp/me"),
                    ("DOCKER_CONFIG", "/home/me/.config/docker"),
                    ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
                ],
            ),
            (
                &[
                    ("LC_ALL", "C"),
                    ("LC_TIME", "en_GB"),
                    ("XDG_RUNTIME_DIR", "/run/user/1000"),
                    ("XDG_CONFIG_HOME", "/home/me/.config"),
                ],
                &[
                    ("LC_ALL", "C"),
                    ("LC_TIME", "en_GB"),
                    ("XDG_RUNTIME_DIR", "/run/user/1000"),
                    ("XDG_CONFIG_HOME", "/home/me/.config"),
                ],
            ),
            (
                &[
                    ("AWS_SECRET_ACCESS_KEY", "hunter2"),
                    ("ALCHEMY_LOG", "debug"),
                    ("SSH_AUTH_SOCK", "/tmp/agent"),
                    ("HTTPS_PROXY", "http://user:pass@proxy"),
                    ("SHELL", "/bin/zsh"),
                ],
                &[],
            ),
            (
                &[
                    ("DOCKER_CONTEXT", "remote"),
                    ("DOCKER_TLS_VERIFY", "1"),
                    ("DOCKER_CERT_PATH", "/certs"),
                    ("COMPOSE_FILE", "other.yaml"),
                    ("COMPOSE_PROJECT_NAME", "other"),
                    ("COMPOSE_PROFILES", "debug"),
                ],
                &[],
            ),
            (
                &[
                    ("DOCKER_HOST", "tcp://elsewhere:2375"),
                    ("COMPOSE_ANSI", "always"),
                    ("DOCKER_CLI_HINTS", "true"),
                ],
                &[],
            ),
            (&[("LC_", "x"), ("XDG_", "y"), ("path", "/bin")], &[]),
            (
                &[("PATHS", "/bin"), ("HOMEDIR", "/home"), ("XLC_ALL", "C")],
                &[],
            ),
            (&[("HOME", "/old"), ("HOME", "/new")], &[("HOME", "/new")]),
        ];
        for (parent, extra) in cases {
            let got = child_env(
                parent
                    .iter()
                    .map(|(k, v)| (OsString::from(k), OsString::from(v))),
                HOST,
            );
            assert_eq!(got, fixed(extra), "{parent:?}");
        }
    }

    #[test]
    fn keeps_non_utf8_values() {
        use std::os::unix::ffi::OsStringExt as _;
        let value = OsString::from_vec(vec![b'/', 0xff, b'x']);
        let got = child_env([(OsString::from("HOME"), value.clone())], HOST);
        assert_eq!(got.get(OsStr::new("HOME")), Some(&value));
    }
}
