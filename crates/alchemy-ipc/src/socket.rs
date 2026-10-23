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
}
