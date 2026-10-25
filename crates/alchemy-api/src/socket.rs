//! Socket path resolution shared by the daemon and its clients.
//!
//! Both sides must derive the same path, so this is the only place it is
//! computed. The daemon is responsible for locking the directory down to
//! `0o700` and the socket to `0o600`; the path alone grants no access.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment variable that overrides the full socket path. Intended
/// for containers, where the socket is usually bind-mounted.
pub const SOCKET_ENV: &str = "ALCHEMY_SOCKET";

/// File name of the daemon socket inside [`runtime_dir`].
pub const SOCKET_FILE: &str = "alchemyd.sock";

/// Where systemd creates the system unit's `RuntimeDirectory=alchemy-<uid>`.
/// It outlives the user's login sessions, unlike `/run/user/<uid>`.
const SYSTEM_RUNTIME_ROOT: &str = "/run";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "only the host's variant is built outside tests")
)]
enum Platform {
    Linux,
    MacOs,
}

#[cfg(target_os = "macos")]
const PLATFORM: Platform = Platform::MacOs;

#[cfg(not(target_os = "macos"))]
const PLATFORM: Platform = Platform::Linux;

/// The per-user runtime directory that holds the daemon socket.
///
/// Uses `$XDG_RUNTIME_DIR/alchemy` when it is set to an absolute path,
/// otherwise `<temp>/alchemy-<uid>`. The temp fallback is per-user on
/// macOS (`$TMPDIR`); on Linux it is shared `/tmp`, which is why the
/// daemon refuses to start if it cannot tighten the directory mode.
pub fn runtime_dir() -> PathBuf {
    runtime_dir_from(
        std::env::var_os("XDG_RUNTIME_DIR"),
        &std::env::temp_dir(),
        rustix::process::getuid().as_raw(),
    )
}

/// The socket path clients connect to and the daemon binds by default:
/// `$ALCHEMY_SOCKET` if set, otherwise `runtime_dir()/alchemyd.sock`.
pub fn default_socket_path() -> PathBuf {
    match std::env::var_os(SOCKET_ENV) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => runtime_dir().join(SOCKET_FILE),
    }
}

/// The sockets a client tries, in order, when nothing names one:
/// `$ALCHEMY_SOCKET` alone if set, otherwise [`default_socket_path`]'s
/// socket and then, on Linux, the system service's
/// `/run/alchemy-<uid>/alchemyd.sock`.
///
/// The daemon never uses this; it listens on [`default_socket_path`].
/// On macOS the launchd service leaves `TMPDIR` unset, so its temp dir is
/// the user's `confstr` one and the first socket already matches.
pub fn socket_candidates() -> Vec<PathBuf> {
    match std::env::var_os(SOCKET_ENV) {
        Some(path) if !path.is_empty() => vec![PathBuf::from(path)],
        _ => socket_candidates_from(
            PLATFORM,
            std::env::var_os("XDG_RUNTIME_DIR"),
            &std::env::temp_dir(),
            rustix::process::getuid().as_raw(),
        ),
    }
}

fn socket_candidates_from(
    platform: Platform,
    xdg: Option<OsString>,
    temp: &Path,
    uid: u32,
) -> Vec<PathBuf> {
    let user = runtime_dir_from(xdg, temp, uid).join(SOCKET_FILE);
    match platform {
        Platform::Linux => vec![
            user,
            Path::new(SYSTEM_RUNTIME_ROOT)
                .join(format!("alchemy-{uid}"))
                .join(SOCKET_FILE),
        ],
        Platform::MacOs => vec![user],
    }
}

fn runtime_dir_from(xdg: Option<OsString>, temp: &Path, uid: u32) -> PathBuf {
    match xdg.map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir.join("alchemy"),
        _ => temp.join(format!("alchemy-{uid}")),
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

    #[test]
    fn prefers_absolute_xdg_runtime_dir() {
        let got = runtime_dir_from(Some("/run/user/1000".into()), Path::new("/tmp"), 1000);
        assert_eq!(got, PathBuf::from("/run/user/1000/alchemy"));
    }

    #[test]
    fn falls_back_to_temp_for_missing_empty_or_relative_xdg() {
        for xdg in [None, Some(OsString::new()), Some("relative/dir".into())] {
            let got = runtime_dir_from(xdg, Path::new("/tmp"), 501);
            assert_eq!(got, PathBuf::from("/tmp/alchemy-501"));
        }
    }

    #[test]
    fn candidates_add_the_system_service_socket_on_linux() {
        let cases: [(Platform, Option<&str>, &[&str]); 6] = [
            (
                Platform::Linux,
                Some("/run/user/1000"),
                &[
                    "/run/user/1000/alchemy/alchemyd.sock",
                    "/run/alchemy-1000/alchemyd.sock",
                ],
            ),
            (
                Platform::Linux,
                None,
                &[
                    "/tmp/alchemy-1000/alchemyd.sock",
                    "/run/alchemy-1000/alchemyd.sock",
                ],
            ),
            (
                Platform::Linux,
                Some(""),
                &[
                    "/tmp/alchemy-1000/alchemyd.sock",
                    "/run/alchemy-1000/alchemyd.sock",
                ],
            ),
            (
                Platform::Linux,
                Some("relative/dir"),
                &[
                    "/tmp/alchemy-1000/alchemyd.sock",
                    "/run/alchemy-1000/alchemyd.sock",
                ],
            ),
            (Platform::MacOs, None, &["/tmp/alchemy-1000/alchemyd.sock"]),
            (
                Platform::MacOs,
                Some("/Users/u/run"),
                &["/Users/u/run/alchemy/alchemyd.sock"],
            ),
        ];
        for (platform, xdg, want) in cases {
            let got =
                socket_candidates_from(platform, xdg.map(OsString::from), Path::new("/tmp"), 1000);
            let want: Vec<PathBuf> = want.iter().map(PathBuf::from).collect();
            assert_eq!(got, want, "{platform:?} {xdg:?}");
        }
    }

    #[test]
    fn the_first_candidate_is_where_the_daemon_listens_by_default() {
        for platform in [Platform::Linux, Platform::MacOs] {
            for xdg in [None, Some(OsString::from("/run/user/7"))] {
                let first = socket_candidates_from(platform, xdg.clone(), Path::new("/tmp"), 7);
                let daemon = runtime_dir_from(xdg, Path::new("/tmp"), 7).join(SOCKET_FILE);
                assert_eq!(first.first(), Some(&daemon));
            }
        }
    }
}
