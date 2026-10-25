//! The autostart service installed on this machine, as the CLI finds it
//! when no alchemyd answers to ask.
//!
//! alchemyd owns its services, so this only reads whether their files
//! exist, at the paths `alchemy_core::lifecycle` gives, to word the
//! unreachable error. The decision is [`installed_service`]; [`find`]
//! reads the environment and the filesystem and calls it.

use std::path::{Path, PathBuf};

use alchemy_api::autostart::AutostartMode;
use alchemy_core::lifecycle::{
    InstalledServices, UserService, log_hint, system_service_path, user_service_path,
};
use alchemy_core::route::HostOs;

use crate::error::InstalledService;

/// The OS this alchemy was built for, which is the one alchemyd runs on:
/// the CLI only reaches a daemon on its own machine.
pub(crate) const fn host_os() -> HostOs {
    if cfg!(target_os = "macos") {
        HostOs::MacOs
    } else {
        HostOs::Linux
    }
}

/// The two service files that can start alchemyd for a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceFiles {
    /// The per-user service, for `login`.
    pub(crate) user: Option<PathBuf>,
    /// The system service, for `boot`.
    pub(crate) system: PathBuf,
}

/// Where the service files are on `os` for the user with `home`, if it is
/// known, and `uid`. Without a home, only the system service is looked
/// for.
pub(crate) fn service_files(
    os: HostOs,
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
    uid: u32,
) -> ServiceFiles {
    ServiceFiles {
        user: home
            .filter(|home| home.is_absolute())
            .map(|home| user_service_path(os, xdg_config_home, home)),
        system: system_service_path(os, uid),
    }
}

/// The service that starts alchemyd, given which of its files exist, or
/// `None` when neither does. The system service wins, as it does for
/// alchemyd.
pub(crate) fn installed_service(
    os: HostOs,
    user: bool,
    system: bool,
    home: Option<&Path>,
    uid: u32,
) -> Option<InstalledService> {
    let user = if user {
        UserService::Current
    } else {
        UserService::Missing
    };
    let mode = InstalledServices { user, system }.mode();
    if mode == AutostartMode::Off {
        return None;
    }
    Some(InstalledService {
        mode,
        logs: log_hint(os, mode, home.unwrap_or(Path::new("~")), uid),
    })
}

/// The service installed for this user, read from `HOME`,
/// `XDG_CONFIG_HOME`, the real UID, and the files on disk.
pub(crate) fn find() -> Option<InstalledService> {
    let os = host_os();
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let uid = rustix::process::getuid().as_raw();
    let files = service_files(os, home.as_deref(), xdg_config_home.as_deref(), uid);
    let user = files.user.as_deref().is_some_and(Path::exists);
    installed_service(os, user, files.system.exists(), home.as_deref(), uid)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use alchemy_api::autostart::LogHint;

    use super::*;

    #[test]
    fn service_files_per_os() {
        let home = Some(Path::new("/home/u"));
        let cases = [
            (
                HostOs::Linux,
                home,
                Some("/home/u/.config/systemd/user/alchemyd.service"),
                "/etc/systemd/system/alchemyd-1000.service",
            ),
            (
                HostOs::MacOs,
                home,
                Some("/home/u/Library/LaunchAgents/computer.alchemy.alchemyd.plist"),
                "/Library/LaunchDaemons/computer.alchemy.alchemyd.1000.plist",
            ),
            (
                HostOs::Linux,
                None,
                None,
                "/etc/systemd/system/alchemyd-1000.service",
            ),
            (
                HostOs::Linux,
                Some(Path::new("relative")),
                None,
                "/etc/systemd/system/alchemyd-1000.service",
            ),
        ];
        for (os, home, user, system) in cases {
            assert_eq!(
                service_files(os, home, None, 1000),
                ServiceFiles {
                    user: user.map(PathBuf::from),
                    system: PathBuf::from(system),
                },
                "{os:?} {home:?}"
            );
        }
    }

    #[test]
    fn the_installed_service_and_its_logs() {
        let home = Some(Path::new("/Users/jo"));
        let file = |path: &str| LogHint::File { path: path.into() };
        let journal = |command: &str| LogHint::Journal {
            command: command.into(),
        };
        let cases = [
            (HostOs::MacOs, false, false, None),
            (HostOs::Linux, false, false, None),
            (
                HostOs::MacOs,
                true,
                false,
                Some((
                    AutostartMode::Login,
                    file("/Users/jo/Library/Logs/Alchemy/alchemyd.log"),
                )),
            ),
            (
                HostOs::MacOs,
                true,
                true,
                Some((
                    AutostartMode::Boot,
                    file("/Library/Logs/Alchemy/alchemyd.501.log"),
                )),
            ),
            (
                HostOs::Linux,
                true,
                false,
                Some((
                    AutostartMode::Login,
                    journal("journalctl --user -u alchemyd.service"),
                )),
            ),
            (
                HostOs::Linux,
                false,
                true,
                Some((
                    AutostartMode::Boot,
                    journal("journalctl -u alchemyd-501.service"),
                )),
            ),
        ];
        for (os, user, system, want) in cases {
            let want = want.map(|(mode, logs)| InstalledService { mode, logs });
            assert_eq!(
                installed_service(os, user, system, home, 501),
                want,
                "{os:?} user {user} system {system}"
            );
        }
    }
}
