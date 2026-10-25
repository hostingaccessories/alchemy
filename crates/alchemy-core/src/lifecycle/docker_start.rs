//! Starting Docker Desktop when someone asks: whether to, and how.
//!
//! Per #12, Alchemy starts Docker Desktop only when asked, and never
//! Docker Engine, which runs as a system service and needs root: it gets
//! the `systemctl` command to run instead.

use std::fmt;

use crate::engine::{EngineFlavor, EngineState};
use crate::route::HostOs;

/// A fixed command that starts Docker Desktop, by absolute path, and the
/// exit status it gives when Docker Desktop isn't installed.
///
/// Neither command waits for the engine to be ready; the engine monitor
/// reports the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DesktopCommand {
    /// The program first, then its arguments.
    pub argv: &'static [&'static str],
    /// The exit status that means Docker Desktop isn't installed.
    pub not_installed: i32,
}

/// Starts Docker Desktop on macOS through Launch Services, in the
/// background. It exits 0 when Docker Desktop is already running, without
/// restarting it, and 1 when it isn't installed (#112).
pub const OPEN_DESKTOP_MACOS: DesktopCommand = DesktopCommand {
    argv: &["/usr/bin/open", "-g", "-b", "com.docker.docker"],
    not_installed: 1,
};

/// Starts Docker Desktop's per-user unit on Linux. It exits 5 when the
/// unit isn't installed (#112), and needs `XDG_RUNTIME_DIR` to reach the
/// user's service manager.
pub const OPEN_DESKTOP_LINUX: DesktopCommand = DesktopCommand {
    argv: &["/usr/bin/systemctl", "--user", "start", "docker-desktop"],
    not_installed: 5,
};

/// The command a person runs to start Docker Engine on Linux.
pub const START_ENGINE_HINT: &str = "sudo systemctl start docker";

/// What a request to start Docker comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DockerStart {
    /// Docker is ready; there is nothing to do.
    AlreadyRunning,
    /// Docker answers its socket but isn't ready yet; it is on its way.
    AlreadyStarting,
    /// Run this command to start Docker Desktop.
    Launch(DesktopCommand),
    /// Alchemy doesn't start this Docker.
    Refuse(StartRefusal),
}

/// Why Alchemy doesn't start Docker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StartRefusal {
    /// Docker Engine runs as a system service and needs root to start.
    DockerEngine,
    /// Another engine, such as Colima or `OrbStack`, starts its own way.
    OtherEngine,
    /// Docker isn't installed.
    NotInstalled,
    /// Docker is configured at an endpoint Alchemy can't use.
    UnsupportedEndpoint,
}

impl StartRefusal {
    /// Why, in a sentence for the UI and CLI.
    pub const fn message(self) -> &'static str {
        match self {
            Self::DockerEngine => {
                "Docker Engine runs as a system service, so starting it needs your password; run the command in a terminal"
            }
            Self::OtherEngine => {
                "Alchemy can only start Docker Desktop; start your Docker engine the way you usually do"
            }
            Self::NotInstalled => "Docker isn't installed; install Docker Desktop first",
            Self::UnsupportedEndpoint => {
                "Docker is set up at an address Alchemy can't use, so it can't start it"
            }
        }
    }

    /// The command to run in a terminal instead, if there is one.
    pub const fn command(self) -> Option<&'static str> {
        match self {
            Self::DockerEngine => Some(START_ENGINE_HINT),
            Self::OtherEngine | Self::NotInstalled | Self::UnsupportedEndpoint => None,
        }
    }
}

impl fmt::Display for StartRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// What to do when asked to start Docker on `os`, given the engine's
/// `state` and `flavor` as the engine status reports them.
///
/// A flavor that is unknown while Docker isn't running means Docker
/// Desktop isn't installed and no engine has been seen this run: on
/// Linux that is Docker Engine, and on macOS some other engine.
pub const fn plan_docker_start(
    os: HostOs,
    state: EngineState,
    flavor: Option<EngineFlavor>,
) -> DockerStart {
    match (state, flavor) {
        (EngineState::Ready, _) => DockerStart::AlreadyRunning,
        (EngineState::Starting, _) => DockerStart::AlreadyStarting,
        (EngineState::Unsupported, _) => DockerStart::Refuse(StartRefusal::UnsupportedEndpoint),
        (
            EngineState::NotRunning | EngineState::NotInstalled,
            Some(EngineFlavor::DockerDesktop),
        ) => DockerStart::Launch(match os {
            HostOs::MacOs => OPEN_DESKTOP_MACOS,
            HostOs::Linux => OPEN_DESKTOP_LINUX,
        }),
        (EngineState::NotInstalled, _) => DockerStart::Refuse(StartRefusal::NotInstalled),
        (EngineState::NotRunning, Some(EngineFlavor::DockerEngine)) => {
            DockerStart::Refuse(StartRefusal::DockerEngine)
        }
        (EngineState::NotRunning, None) => match os {
            HostOs::Linux => DockerStart::Refuse(StartRefusal::DockerEngine),
            HostOs::MacOs => DockerStart::Refuse(StartRefusal::OtherEngine),
        },
        (EngineState::NotRunning, Some(EngineFlavor::Other)) => {
            DockerStart::Refuse(StartRefusal::OtherEngine)
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
    fn decides_by_os_state_and_flavor() {
        use DockerStart::{AlreadyRunning, AlreadyStarting, Launch, Refuse};
        use EngineFlavor::{DockerDesktop, DockerEngine, Other};
        use EngineState::{NotInstalled, NotRunning, Ready, Starting, Unsupported};
        use HostOs::{Linux, MacOs};
        use StartRefusal::{NotInstalled as Missing, OtherEngine, UnsupportedEndpoint};
        let cases = [
            (
                MacOs,
                NotRunning,
                Some(DockerDesktop),
                Launch(OPEN_DESKTOP_MACOS),
            ),
            (
                Linux,
                NotRunning,
                Some(DockerDesktop),
                Launch(OPEN_DESKTOP_LINUX),
            ),
            (
                MacOs,
                NotInstalled,
                Some(DockerDesktop),
                Launch(OPEN_DESKTOP_MACOS),
            ),
            (MacOs, Ready, Some(DockerDesktop), AlreadyRunning),
            (Linux, Ready, Some(DockerEngine), AlreadyRunning),
            (MacOs, Starting, Some(DockerDesktop), AlreadyStarting),
            (Linux, Starting, None, AlreadyStarting),
            (
                Linux,
                NotRunning,
                Some(DockerEngine),
                Refuse(StartRefusal::DockerEngine),
            ),
            (Linux, NotRunning, None, Refuse(StartRefusal::DockerEngine)),
            (MacOs, NotRunning, None, Refuse(OtherEngine)),
            (MacOs, NotRunning, Some(Other), Refuse(OtherEngine)),
            (Linux, NotRunning, Some(Other), Refuse(OtherEngine)),
            (MacOs, NotInstalled, None, Refuse(Missing)),
            (Linux, NotInstalled, None, Refuse(Missing)),
            (
                MacOs,
                Unsupported,
                Some(DockerDesktop),
                Refuse(UnsupportedEndpoint),
            ),
        ];
        for (os, state, flavor, want) in cases {
            assert_eq!(
                plan_docker_start(os, state, flavor),
                want,
                "{os:?} {state:?} {flavor:?}"
            );
        }
    }

    #[test]
    fn only_docker_engine_gets_a_command() {
        use StartRefusal::{DockerEngine, NotInstalled, OtherEngine, UnsupportedEndpoint};
        assert_eq!(DockerEngine.command(), Some("sudo systemctl start docker"));
        for refusal in [OtherEngine, NotInstalled, UnsupportedEndpoint] {
            assert_eq!(refusal.command(), None, "{refusal:?}");
        }
    }

    #[test]
    fn launch_commands_are_absolute() {
        for command in [OPEN_DESKTOP_MACOS, OPEN_DESKTOP_LINUX] {
            assert!(command.argv[0].starts_with('/'), "{command:?}");
        }
    }
}
