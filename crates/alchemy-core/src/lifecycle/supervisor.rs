//! What runs this alchemyd, and how each autostart step is carried out
//! from inside it.
//!
//! A mode switch can be asked of an alchemyd that the service being
//! removed is running. Removing it stops that process, so the steps are
//! adjusted: the per-user service is never started over itself, turning
//! autostart off leaves the running process alone, and handing over to
//! the system service stops it only after it has answered. The service
//! that starts next waits for the state directory's lock, so the two never
//! run against the same state.

use serde::{Deserialize, Serialize};

use super::autostart::{AutostartMode, AutostartStep};
use super::service_file::{LAUNCH_AGENT_LABEL, SYSTEMD_USER_UNIT};

/// What started this alchemyd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Supervisor {
    /// Nothing Alchemy installed: a terminal, a script, or a debugger.
    Manual,
    /// The per-user service, for autostart `login`.
    UserService,
    /// The system service, for autostart `boot`.
    SystemService,
}

impl Supervisor {
    /// The autostart mode whose service this is, `off` for
    /// [`Self::Manual`].
    pub const fn mode(self) -> AutostartMode {
        match self {
            Self::Manual => AutostartMode::Off,
            Self::UserService => AutostartMode::Login,
            Self::SystemService => AutostartMode::Boot,
        }
    }

    /// Whether a service manager runs this alchemyd, so it should wait
    /// for the state directory instead of giving up when another alchemyd
    /// holds it, as during a handover.
    pub const fn waits_for_state(self) -> bool {
        !matches!(self, Self::Manual)
    }
}

/// The supervisor from `/proc/self/cgroup` on Linux, for the user with
/// `uid`.
///
/// systemd puts every service in a cgroup named after its unit: the user
/// unit under `user@<uid>.service`, the system unit (named as M8's helper
/// installs it) under `system.slice`. Both cgroup v2 (`0::/path`) and the
/// v1 `name=systemd` hierarchy are read.
pub fn supervisor_from_cgroup(cgroup: &str, uid: u32) -> Supervisor {
    let system_unit = format!("alchemyd-{uid}.service");
    let user_manager = format!("user@{uid}.service");
    for line in cgroup.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !(controllers.is_empty() || controllers == "name=systemd") {
            continue;
        }
        let mut parts = path.trim_end().rsplit('/');
        let unit = parts.next().unwrap_or_default();
        let parents: Vec<&str> = parts.collect();
        if unit == system_unit && parents.contains(&"system.slice") {
            return Supervisor::SystemService;
        }
        if unit == SYSTEMD_USER_UNIT && parents.contains(&user_manager.as_str()) {
            return Supervisor::UserService;
        }
    }
    Supervisor::Manual
}

/// The supervisor from `XPC_SERVICE_NAME` on macOS, which launchd sets to
/// the label of the job it started, for the user with `uid`. The system
/// service's label is the launch agent's followed by `.<uid>`.
pub fn supervisor_from_launchd(label: Option<&str>, uid: u32) -> Supervisor {
    match label {
        Some(label) if label == LAUNCH_AGENT_LABEL => Supervisor::UserService,
        Some(label) if label == format!("{LAUNCH_AGENT_LABEL}.{uid}") => Supervisor::SystemService,
        _ => Supervisor::Manual,
    }
}

/// How the daemon carries out one [`AutostartStep`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceCall {
    /// Write the per-user service file and register it; with `start`,
    /// also start it now.
    InstallUser {
        /// Whether to start it now.
        start: bool,
    },
    /// Unregister the per-user service and delete its file; with `stop`,
    /// also stop it if it runs.
    RemoveUser {
        /// Whether to stop it.
        stop: bool,
    },
    /// Remove the per-user service that runs this alchemyd, after
    /// answering: it stops, and the system service's alchemyd, waiting
    /// for the state directory, takes over.
    HandOver,
    /// Install the system service through the helper.
    InstallSystem,
    /// Remove the system service through the helper. If it runs this
    /// alchemyd, that stops it, and the per-user service's alchemyd
    /// takes over when the target is `login`.
    RemoveSystem,
}

/// How to carry out `step` toward `target` in an alchemyd that
/// `supervisor` runs.
///
/// The per-user service is started at once, unless it is this process:
/// another alchemyd started now waits for this one's lock and takes over
/// when it exits. Removing it from inside itself stops it only when the
/// system service is taking over; turning autostart off leaves it
/// running until it exits.
pub const fn service_call(
    step: AutostartStep,
    supervisor: Supervisor,
    target: AutostartMode,
) -> ServiceCall {
    let inside = matches!(supervisor, Supervisor::UserService);
    match step {
        AutostartStep::InstallUserService => ServiceCall::InstallUser { start: !inside },
        AutostartStep::RemoveUserService if inside && matches!(target, AutostartMode::Boot) => {
            ServiceCall::HandOver
        }
        AutostartStep::RemoveUserService => ServiceCall::RemoveUser { stop: !inside },
        AutostartStep::InstallSystemService => ServiceCall::InstallSystem,
        AutostartStep::RemoveSystemService => ServiceCall::RemoveSystem,
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
    use Supervisor::{Manual, SystemService, UserService};

    #[test]
    fn supervisor_from_cgroups() {
        let cases = [
            ("0::/system.slice/alchemyd-1000.service\n", SystemService),
            (
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/alchemyd.service\n",
                UserService,
            ),
            (
                "12:pids:/user.slice\n1:name=systemd:/system.slice/alchemyd-1000.service\n0::/\n",
                SystemService,
            ),
            ("0::/user.slice/user-1000.slice/session-3.scope\n", Manual),
            ("0::/system.slice/alchemyd-1001.service\n", Manual),
            ("0::/system.slice/alchemyd.service\n", Manual),
            (
                "0::/user.slice/user-1001.slice/user@1001.service/app.slice/alchemyd.service\n",
                Manual,
            ),
            ("0::/user.slice/alchemyd.service\n", Manual),
            ("3:cpu:/system.slice/alchemyd-1000.service\n", Manual),
            ("0::/docker/abc\n", Manual),
            ("", Manual),
            ("garbage", Manual),
        ];
        for (cgroup, want) in cases {
            assert_eq!(supervisor_from_cgroup(cgroup, 1000), want, "{cgroup:?}");
        }
    }

    #[test]
    fn supervisor_from_launchd_labels() {
        let cases = [
            (Some("computer.alchemy.alchemyd"), UserService),
            (Some("computer.alchemy.alchemyd.501"), SystemService),
            (Some("computer.alchemy.alchemyd.502"), Manual),
            (Some("0"), Manual),
            (Some("application.com.apple.Terminal.123"), Manual),
            (None, Manual),
        ];
        for (label, want) in cases {
            assert_eq!(supervisor_from_launchd(label, 501), want, "{label:?}");
        }
    }

    #[test]
    fn supervisor_modes() {
        let cases = [
            (Manual, Off, false),
            (UserService, Login, true),
            (SystemService, Boot, true),
        ];
        for (supervisor, mode, waits) in cases {
            assert_eq!(supervisor.mode(), mode);
            assert_eq!(supervisor.waits_for_state(), waits);
        }
    }

    #[test]
    fn service_calls() {
        let cases = [
            (
                InstallUserService,
                Manual,
                Login,
                ServiceCall::InstallUser { start: true },
            ),
            (
                InstallUserService,
                SystemService,
                Login,
                ServiceCall::InstallUser { start: true },
            ),
            (
                InstallUserService,
                UserService,
                Login,
                ServiceCall::InstallUser { start: false },
            ),
            (
                RemoveUserService,
                Manual,
                Off,
                ServiceCall::RemoveUser { stop: true },
            ),
            (
                RemoveUserService,
                Manual,
                Boot,
                ServiceCall::RemoveUser { stop: true },
            ),
            (
                RemoveUserService,
                SystemService,
                Boot,
                ServiceCall::RemoveUser { stop: true },
            ),
            (
                RemoveUserService,
                UserService,
                Off,
                ServiceCall::RemoveUser { stop: false },
            ),
            (RemoveUserService, UserService, Boot, ServiceCall::HandOver),
            (
                InstallSystemService,
                UserService,
                Boot,
                ServiceCall::InstallSystem,
            ),
            (
                RemoveSystemService,
                SystemService,
                Login,
                ServiceCall::RemoveSystem,
            ),
        ];
        for (step, supervisor, target, want) in cases {
            assert_eq!(
                service_call(step, supervisor, target),
                want,
                "{step:?} {supervisor:?} {target}"
            );
        }
    }

    #[test]
    fn supervisor_wire_names() {
        for (supervisor, name) in [
            (Manual, "manual"),
            (UserService, "user_service"),
            (SystemService, "system_service"),
        ] {
            let json = format!("\"{name}\"");
            assert_eq!(serde_json::to_string(&supervisor).unwrap(), json);
            assert_eq!(
                serde_json::from_str::<Supervisor>(&json).unwrap(),
                supervisor
            );
        }
    }
}
