//! Where the state database lives.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment variable `alchemyd` reads for `--state-dir`, for tests and
/// for running a second daemon beside the real one.
pub const STATE_DIR_ENV: &str = "ALCHEMY_STATE_DIR";

/// File name of the SQLite database inside the state directory.
pub const DB_FILE: &str = "alchemy.db";

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

/// The per-user state directory: `$XDG_STATE_HOME/alchemy` (falling back
/// to `~/.local/state/alchemy`) on Linux and
/// `~/Library/Application Support/Alchemy` on macOS.
///
/// `None` when no usable directory can be derived, such as when `HOME` is
/// unset or relative.
pub fn default_state_dir() -> Option<PathBuf> {
    state_dir_from(
        PLATFORM,
        std::env::var_os("XDG_STATE_HOME"),
        std::env::home_dir(),
    )
}

fn state_dir_from(
    platform: Platform,
    xdg_state_home: Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let home = home.filter(|home| home.is_absolute());
    match platform {
        Platform::Linux => match xdg_state_home.map(PathBuf::from) {
            Some(dir) if dir.is_absolute() => Some(dir.join("alchemy")),
            _ => home.map(|home| home.join(".local/state/alchemy")),
        },
        Platform::MacOs => {
            home.map(|home| home.join(Path::new("Library/Application Support/Alchemy")))
        }
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
    fn state_dir_per_platform() {
        let home = || Some(PathBuf::from("/home/u"));
        let cases = [
            (
                "linux xdg",
                Platform::Linux,
                Some("/xdg/state"),
                home(),
                Some("/xdg/state/alchemy"),
            ),
            (
                "linux no xdg",
                Platform::Linux,
                None,
                home(),
                Some("/home/u/.local/state/alchemy"),
            ),
            (
                "linux empty xdg",
                Platform::Linux,
                Some(""),
                home(),
                Some("/home/u/.local/state/alchemy"),
            ),
            (
                "linux relative xdg",
                Platform::Linux,
                Some("rel"),
                home(),
                Some("/home/u/.local/state/alchemy"),
            ),
            (
                "linux xdg without home",
                Platform::Linux,
                Some("/xdg"),
                None,
                Some("/xdg/alchemy"),
            ),
            ("linux nothing", Platform::Linux, None, None, None),
            (
                "macos",
                Platform::MacOs,
                Some("/xdg"),
                home(),
                Some("/home/u/Library/Application Support/Alchemy"),
            ),
            (
                "macos relative home",
                Platform::MacOs,
                None,
                Some(PathBuf::from("rel")),
                None,
            ),
        ];
        for (name, platform, xdg, home, want) in cases {
            let got = state_dir_from(platform, xdg.map(OsString::from), home);
            assert_eq!(got, want.map(PathBuf::from), "{name}");
        }
    }
}
