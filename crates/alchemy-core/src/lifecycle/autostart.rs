//! Autostart: whether alchemyd starts on its own, and the steps to change
//! it.
//!
//! Per #12 there is one setting with three modes, and only one service is
//! ever installed. Going to `boot` installs the system service before it
//! removes the per-user one, and going back to `login` installs the
//! per-user one before it removes the system service, so a declined
//! password prompt leaves the old mode in place rather than none.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::service_file::{SYSTEMD_USER_UNIT, log_path, system_log_path};
use crate::privilege::ServiceFacts;
use crate::route::HostOs;

/// When alchemyd starts on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutostartMode {
    /// Never: alchemyd runs only when started by hand.
    Off,
    /// At login, as a per-user service (a launch agent or a
    /// `systemd --user` unit) installed without root.
    Login,
    /// At boot, as M8's system service, installed through the helper.
    Boot,
}

/// Why an autostart mode name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown autostart mode {0:?}; use off, login, or boot")]
pub struct AutostartModeError(pub String);

impl AutostartMode {
    /// Every mode, in display order.
    pub const ALL: [Self; 3] = [Self::Off, Self::Login, Self::Boot];

    /// The wire name, such as `login`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Login => "login",
            Self::Boot => "boot",
        }
    }

    /// Whether this mode wants the system service, which is
    /// [`ServiceFacts::wanted`].
    pub const fn wants_system_service(self) -> bool {
        matches!(self, Self::Boot)
    }

    /// The system service's facts in this mode, given whether it is
    /// installed.
    pub const fn service_facts(self, installed: bool) -> ServiceFacts {
        ServiceFacts {
            installed,
            wanted: self.wants_system_service(),
        }
    }
}

impl fmt::Display for AutostartMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AutostartMode {
    type Err = AutostartModeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == s)
            .ok_or_else(|| AutostartModeError(s.to_owned()))
    }
}

/// The per-user service file, compared with what
/// [`launch_agent_plist`](super::launch_agent_plist) or
/// [`systemd_user_unit`](super::systemd_user_unit) would write now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserService {
    /// There is none.
    Missing,
    /// It holds exactly what would be written now.
    Current,
    /// It differs, such as after alchemyd moved, so installing again
    /// rewrites it.
    Outdated,
}

impl UserService {
    /// Compare the file `on_disk`, if there is one, with `rendered`, what
    /// would be written now. A file that can't be rendered any more, as
    /// after alchemyd moved to a path a unit can't hold, is outdated.
    pub fn compare(on_disk: Option<&str>, rendered: Option<&str>) -> Self {
        match (on_disk, rendered) {
            (None, _) => Self::Missing,
            (Some(file), Some(want)) if file == want => Self::Current,
            (Some(_), _) => Self::Outdated,
        }
    }
}

/// Which services are installed, as the daemon read them from disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstalledServices {
    /// The per-user service.
    pub user: UserService,
    /// Whether the system service's unit or plist exists, as
    /// [`ServiceFacts::installed`].
    pub system: bool,
}

impl InstalledServices {
    /// The mode these files put alchemyd in. The system service wins if
    /// both are installed, as after a switch was interrupted.
    pub const fn mode(self) -> AutostartMode {
        match self {
            Self { system: true, .. } => AutostartMode::Boot,
            Self {
                user: UserService::Current | UserService::Outdated,
                ..
            } => AutostartMode::Login,
            Self {
                user: UserService::Missing,
                ..
            } => AutostartMode::Off,
        }
    }
}

/// One change to the installed services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AutostartStep {
    /// Write the per-user service file, then load and start it.
    InstallUserService,
    /// Stop and unload the per-user service, then delete its file.
    RemoveUserService,
    /// Run `install-service` through the helper, behind a prompt.
    InstallSystemService,
    /// Run `remove-service` through the helper, behind a prompt.
    RemoveSystemService,
}

/// The steps, in order, that take `installed` to `target`.
///
/// The new mode's service is installed before the old one is removed. An
/// outdated per-user file is rewritten when `target` is `login`. The
/// system service is never installed again once it is, since that would
/// prompt for nothing; the helper's own install is idempotent if it is.
pub fn autostart_steps(installed: InstalledServices, target: AutostartMode) -> Vec<AutostartStep> {
    let has_user = !matches!(installed.user, UserService::Missing);
    let mut steps = Vec::with_capacity(2);
    match target {
        AutostartMode::Off => {
            if has_user {
                steps.push(AutostartStep::RemoveUserService);
            }
            if installed.system {
                steps.push(AutostartStep::RemoveSystemService);
            }
        }
        AutostartMode::Login => {
            if !matches!(installed.user, UserService::Current) {
                steps.push(AutostartStep::InstallUserService);
            }
            if installed.system {
                steps.push(AutostartStep::RemoveSystemService);
            }
        }
        AutostartMode::Boot => {
            if !installed.system {
                steps.push(AutostartStep::InstallSystemService);
            }
            if has_user {
                steps.push(AutostartStep::RemoveUserService);
            }
        }
    }
    steps
}

/// Which services can be installed on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModeSupport {
    /// The per-user service: on Linux, a `systemd --user` manager runs
    /// and alchemyd's path fits in a unit; on macOS, the user has a home.
    pub user_service: bool,
    /// The system service: the helper can install it, from a prompt or a
    /// terminal.
    pub system_service: bool,
}

/// The modes alchemyd can be switched to, in display order: `off`
/// always, `login` and `boot` when `support` says so, and `current`
/// whatever it says, so the mode in place is always listed.
pub fn available_modes(current: AutostartMode, support: ModeSupport) -> Vec<AutostartMode> {
    AutostartMode::ALL
        .into_iter()
        .filter(|mode| {
            *mode == current
                || match mode {
                    AutostartMode::Off => true,
                    AutostartMode::Login => support.user_service,
                    AutostartMode::Boot => support.system_service,
                }
        })
        .collect()
}

/// Where alchemyd's logs go.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogHint {
    /// A file, as from a launch agent or launch daemon on macOS.
    File {
        /// Its absolute path.
        path: PathBuf,
    },
    /// The systemd journal, read with `command`.
    Journal {
        /// The `journalctl` command line, such as
        /// `journalctl --user -u alchemyd.service`.
        command: String,
    },
    /// The terminal alchemyd was started from, when no service runs it.
    Terminal,
}

/// Where logs go for alchemyd in `mode` on `os`, for the user with `home`
/// and `uid`.
///
/// The launch agent logs to [`log_path`] and the launch daemon to
/// [`system_log_path`]. A system unit is named `alchemyd-<uid>.service`,
/// as M8's helper installs it.
pub fn log_hint(os: HostOs, mode: AutostartMode, home: &Path, uid: u32) -> LogHint {
    match (os, mode) {
        (_, AutostartMode::Off) => LogHint::Terminal,
        (HostOs::MacOs, AutostartMode::Login) => LogHint::File {
            path: log_path(home),
        },
        (HostOs::MacOs, AutostartMode::Boot) => LogHint::File {
            path: system_log_path(uid),
        },
        (HostOs::Linux, AutostartMode::Login) => LogHint::Journal {
            command: format!("journalctl --user -u {SYSTEMD_USER_UNIT}"),
        },
        (HostOs::Linux, AutostartMode::Boot) => LogHint::Journal {
            command: format!("journalctl -u alchemyd-{uid}.service"),
        },
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
    use AutostartMode::{Boot, Login, Off};
    use AutostartStep::{
        InstallSystemService, InstallUserService, RemoveSystemService, RemoveUserService,
    };
    use UserService::{Current, Missing, Outdated};

    const fn installed(user: UserService, system: bool) -> InstalledServices {
        InstalledServices { user, system }
    }

    #[test]
    fn steps_to_each_mode() {
        let cases: [(InstalledServices, AutostartMode, &[AutostartStep]); 18] = [
            (installed(Missing, false), Off, &[]),
            (installed(Missing, false), Login, &[InstallUserService]),
            (installed(Missing, false), Boot, &[InstallSystemService]),
            (installed(Current, false), Off, &[RemoveUserService]),
            (installed(Current, false), Login, &[]),
            (
                installed(Current, false),
                Boot,
                &[InstallSystemService, RemoveUserService],
            ),
            (installed(Outdated, false), Off, &[RemoveUserService]),
            (installed(Outdated, false), Login, &[InstallUserService]),
            (
                installed(Outdated, false),
                Boot,
                &[InstallSystemService, RemoveUserService],
            ),
            (installed(Missing, true), Off, &[RemoveSystemService]),
            (
                installed(Missing, true),
                Login,
                &[InstallUserService, RemoveSystemService],
            ),
            (installed(Missing, true), Boot, &[]),
            (
                installed(Current, true),
                Off,
                &[RemoveUserService, RemoveSystemService],
            ),
            (installed(Current, true), Login, &[RemoveSystemService]),
            (installed(Current, true), Boot, &[RemoveUserService]),
            (
                installed(Outdated, true),
                Off,
                &[RemoveUserService, RemoveSystemService],
            ),
            (
                installed(Outdated, true),
                Login,
                &[InstallUserService, RemoveSystemService],
            ),
            (installed(Outdated, true), Boot, &[RemoveUserService]),
        ];
        for (from, target, want) in cases {
            assert_eq!(autostart_steps(from, target), want, "{from:?} -> {target}");
        }
    }

    #[test]
    fn steps_reach_the_target_with_one_service() {
        for user in [Missing, Current, Outdated] {
            for system in [false, true] {
                for target in AutostartMode::ALL {
                    let mut now = installed(user, system);
                    for step in autostart_steps(now, target) {
                        match step {
                            InstallUserService => now.user = Current,
                            RemoveUserService => now.user = Missing,
                            InstallSystemService => now.system = true,
                            RemoveSystemService => now.system = false,
                        }
                    }
                    assert_eq!(now.mode(), target, "{user:?} {system} -> {target}");
                    assert!(
                        !(now.system && now.user != Missing),
                        "{user:?} {system} -> {target}: both installed"
                    );
                    assert_eq!(
                        autostart_steps(now, target),
                        Vec::new(),
                        "{user:?} {system} -> {target}: again"
                    );
                }
            }
        }
    }

    #[test]
    fn mode_from_installed_files() {
        let cases = [
            (installed(Missing, false), Off),
            (installed(Current, false), Login),
            (installed(Outdated, false), Login),
            (installed(Missing, true), Boot),
            (installed(Current, true), Boot),
        ];
        for (files, want) in cases {
            assert_eq!(files.mode(), want, "{files:?}");
        }
    }

    #[test]
    fn boot_wants_the_system_service() {
        let cases = [(Off, false), (Login, false), (Boot, true)];
        for (mode, wanted) in cases {
            assert_eq!(mode.wants_system_service(), wanted, "{mode}");
            for installed in [false, true] {
                assert_eq!(
                    mode.service_facts(installed),
                    ServiceFacts { installed, wanted },
                    "{mode} {installed}"
                );
            }
        }
    }

    #[test]
    fn mode_names_round_trip() {
        for (mode, name) in [(Off, "off"), (Login, "login"), (Boot, "boot")] {
            assert_eq!(mode.as_str(), name);
            assert_eq!(mode.to_string(), name);
            assert_eq!(name.parse::<AutostartMode>().unwrap(), mode);
            let json = format!("\"{name}\"");
            assert_eq!(serde_json::to_string(&mode).unwrap(), json);
            assert_eq!(serde_json::from_str::<AutostartMode>(&json).unwrap(), mode);
        }
        for bad in ["", "Login", "system", "boot "] {
            assert_eq!(
                bad.parse::<AutostartMode>(),
                Err(AutostartModeError(bad.to_owned()))
            );
        }
    }

    #[test]
    fn user_service_from_files() {
        let cases = [
            (None, Some("unit"), Missing),
            (None, None, Missing),
            (Some("unit"), Some("unit"), Current),
            (Some("old"), Some("unit"), Outdated),
            (Some("unit"), None, Outdated),
        ];
        for (on_disk, rendered, want) in cases {
            assert_eq!(
                UserService::compare(on_disk, rendered),
                want,
                "{on_disk:?} {rendered:?}"
            );
        }
    }

    #[test]
    fn modes_available() {
        let support = |user_service, system_service| ModeSupport {
            user_service,
            system_service,
        };
        let cases = [
            (Off, support(true, true), vec![Off, Login, Boot]),
            (Off, support(true, false), vec![Off, Login]),
            (Off, support(false, true), vec![Off, Boot]),
            (Off, support(false, false), vec![Off]),
            (Login, support(false, false), vec![Off, Login]),
            (Boot, support(true, false), vec![Off, Login, Boot]),
            (Boot, support(false, false), vec![Off, Boot]),
        ];
        for (current, support, want) in cases {
            assert_eq!(
                available_modes(current, support),
                want,
                "{current} {support:?}"
            );
        }
    }

    #[test]
    fn log_hints() {
        let home = Path::new("/Users/jo");
        let log = LogHint::File {
            path: PathBuf::from("/Users/jo/Library/Logs/Alchemy/alchemyd.log"),
        };
        let cases = [
            (HostOs::MacOs, Off, LogHint::Terminal),
            (HostOs::MacOs, Login, log),
            (
                HostOs::MacOs,
                Boot,
                LogHint::File {
                    path: PathBuf::from("/Library/Logs/Alchemy/alchemyd.501.log"),
                },
            ),
            (HostOs::Linux, Off, LogHint::Terminal),
            (
                HostOs::Linux,
                Login,
                LogHint::Journal {
                    command: "journalctl --user -u alchemyd.service".into(),
                },
            ),
            (
                HostOs::Linux,
                Boot,
                LogHint::Journal {
                    command: "journalctl -u alchemyd-501.service".into(),
                },
            ),
        ];
        for (os, mode, want) in cases {
            assert_eq!(log_hint(os, mode, home, 501), want, "{os:?} {mode}");
        }
    }

    #[test]
    fn log_hint_wire_shape() {
        let cases = [
            (
                LogHint::File {
                    path: "/Users/jo/Library/Logs/Alchemy/alchemyd.log".into(),
                },
                serde_json::json!({
                    "kind": "file",
                    "path": "/Users/jo/Library/Logs/Alchemy/alchemyd.log",
                }),
            ),
            (
                LogHint::Journal {
                    command: "journalctl --user -u alchemyd.service".into(),
                },
                serde_json::json!({
                    "kind": "journal",
                    "command": "journalctl --user -u alchemyd.service",
                }),
            ),
            (LogHint::Terminal, serde_json::json!({ "kind": "terminal" })),
        ];
        for (hint, json) in cases {
            assert_eq!(serde_json::to_value(&hint).unwrap(), json);
            assert_eq!(serde_json::from_value::<LogHint>(json).unwrap(), hint);
        }
    }
}
