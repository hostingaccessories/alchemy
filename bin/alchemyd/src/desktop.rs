//! Starting Docker Desktop when someone asks, for
//! `POST /api/engine/start`.
//!
//! Whether to start it, and the fixed command that does, are decided by
//! [`plan_docker_start`]. The command runs through a [`DesktopLauncher`]:
//! [`SystemLauncher`] in the daemon, [`FakeLauncher`] in tests, so no test
//! ever starts Docker Desktop. Neither command waits for the engine to be
//! ready, so the request answers at once and the engine monitor reports
//! the rest.

use std::ffi::OsString;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use alchemy_api::engine::{EngineStart, EngineStartResult};
use alchemy_core::engine::EngineStatus;
use alchemy_core::lifecycle::{DesktopCommand, DockerStart, plan_docker_start};
use alchemy_core::route::HostOs;
use futures_util::future::BoxFuture;

/// How long a launch command may take before it counts as failed.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The environment a launch command keeps, apart from
/// `XDG_RUNTIME_DIR`: what `systemctl --user` needs to reach the user's
/// service manager, and who the user is.
const KEPT_ENV: &[&str] = &["HOME", "USER", "LOGNAME", "DBUS_SESSION_BUS_ADDRESS"];

/// A launcher shared by both listeners.
pub type SharedLauncher = Arc<dyn DesktopLauncher>;

/// Runs the fixed command that starts Docker Desktop.
pub trait DesktopLauncher: Send + Sync + fmt::Debug {
    /// Run `command` and wait for it to exit.
    fn launch(&self, command: DesktopCommand) -> BoxFuture<'_, Result<(), LaunchError>>;
}

/// Why a launch command failed.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// The command was empty.
    #[error("no command to start Docker Desktop with")]
    Empty,
    /// The command says Docker Desktop isn't installed.
    #[error("Docker Desktop isn't installed")]
    NotInstalled,
    /// The program could not be run.
    #[error("couldn't run {program}: {source}")]
    Spawn {
        /// The program.
        program: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The program exited unsuccessfully.
    #[error("{program} couldn't start Docker Desktop: {detail}")]
    Failed {
        /// The program.
        program: String,
        /// What it printed on stderr, or its exit status.
        detail: String,
    },
    /// The program took longer than [`LAUNCH_TIMEOUT`].
    #[error("{program} took too long to start Docker Desktop")]
    TimedOut {
        /// The program.
        program: String,
    },
}

/// What `POST /api/engine/start` answers on `os` for an engine in
/// `status`, launching Docker Desktop through `launcher` when it should.
pub async fn start_docker(
    launcher: &dyn DesktopLauncher,
    os: HostOs,
    status: &EngineStatus,
) -> Result<EngineStart, LaunchError> {
    let answer = |result, message: &str, command: Option<&str>| EngineStart {
        result,
        message: message.to_owned(),
        command: command.map(str::to_owned),
    };
    Ok(match plan_docker_start(os, status.state, status.flavor) {
        DockerStart::AlreadyRunning => {
            answer(EngineStartResult::AlreadyRunning, "Docker is running", None)
        }
        DockerStart::AlreadyStarting => {
            answer(EngineStartResult::Starting, "Docker is starting", None)
        }
        DockerStart::Launch(command) => {
            launcher.launch(command).await?;
            tracing::info!(command = command.argv.join(" "), "starting Docker Desktop");
            answer(
                EngineStartResult::Starting,
                "Docker Desktop is starting",
                None,
            )
        }
        DockerStart::Refuse(refusal) => answer(
            EngineStartResult::Unsupported,
            refusal.message(),
            refusal.command(),
        ),
    })
}

/// The OS this alchemyd was built for.
pub const fn host_os() -> HostOs {
    if cfg!(target_os = "macos") {
        HostOs::MacOs
    } else {
        HostOs::Linux
    }
}

/// `XDG_RUNTIME_DIR` for a launch command: the daemon's own when it is
/// set, else the one systemd gives user `uid`, since `systemctl --user`
/// fails without it.
fn runtime_dir(own: Option<OsString>, uid: u32) -> OsString {
    own.filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| format!("/run/user/{uid}").into())
}

/// Runs launch commands as child processes, with a cleared environment
/// apart from [`KEPT_ENV`] and `XDG_RUNTIME_DIR`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemLauncher;

impl DesktopLauncher for SystemLauncher {
    fn launch(&self, desktop: DesktopCommand) -> BoxFuture<'_, Result<(), LaunchError>> {
        Box::pin(async move {
            let (&program, args) = desktop.argv.split_first().ok_or(LaunchError::Empty)?;
            let mut command = tokio::process::Command::new(program);
            command
                .args(args)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("LC_ALL", "C")
                .env(
                    "XDG_RUNTIME_DIR",
                    runtime_dir(
                        std::env::var_os("XDG_RUNTIME_DIR"),
                        rustix::process::getuid().as_raw(),
                    ),
                )
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .kill_on_drop(true);
            for name in KEPT_ENV {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            let output = tokio::time::timeout(LAUNCH_TIMEOUT, command.output())
                .await
                .map_err(|_| LaunchError::TimedOut {
                    program: program.to_owned(),
                })?
                .map_err(|source| LaunchError::Spawn {
                    program: program.to_owned(),
                    source,
                })?;
            if output.status.success() {
                return Ok(());
            }
            if output.status.code() == Some(desktop.not_installed) {
                return Err(LaunchError::NotInstalled);
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map_or_else(|| output.status.to_string(), str::to_owned);
            Err(LaunchError::Failed {
                program: program.to_owned(),
                detail,
            })
        })
    }
}

/// A launcher that records each command and runs nothing.
#[derive(Debug, Default)]
pub struct FakeLauncher {
    launched: Mutex<Vec<Vec<String>>>,
    not_installed: bool,
}

impl FakeLauncher {
    /// A launcher whose every command succeeds.
    pub fn new() -> Self {
        Self::default()
    }

    /// A launcher whose every command says Docker Desktop isn't
    /// installed.
    #[must_use]
    pub fn not_installed() -> Self {
        Self {
            launched: Mutex::new(Vec::new()),
            not_installed: true,
        }
    }

    /// Every command asked for, oldest first.
    pub fn launched(&self) -> Vec<Vec<String>> {
        self.launched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl DesktopLauncher for FakeLauncher {
    fn launch(&self, command: DesktopCommand) -> BoxFuture<'_, Result<(), LaunchError>> {
        Box::pin(async move {
            self.launched
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(command.argv.iter().map(|&arg| arg.to_owned()).collect());
            if self.not_installed {
                Err(LaunchError::NotInstalled)
            } else {
                Ok(())
            }
        })
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
    use alchemy_core::engine::{EngineFlavor, EngineState};
    use alchemy_core::lifecycle::{OPEN_DESKTOP_LINUX, OPEN_DESKTOP_MACOS};

    use super::*;

    fn status(state: EngineState, flavor: Option<EngineFlavor>) -> EngineStatus {
        let mut status = EngineStatus::new(state);
        status.flavor = flavor;
        status
    }

    #[tokio::test]
    async fn launches_only_docker_desktop_that_is_not_running() {
        use EngineFlavor::{DockerDesktop, DockerEngine};
        use EngineStartResult::{AlreadyRunning, Starting as Coming, Unsupported};
        use EngineState::{NotRunning, Ready, Starting};
        let cases = [
            (
                HostOs::MacOs,
                NotRunning,
                Some(DockerDesktop),
                Coming,
                Some(OPEN_DESKTOP_MACOS),
            ),
            (
                HostOs::Linux,
                NotRunning,
                Some(DockerDesktop),
                Coming,
                Some(OPEN_DESKTOP_LINUX),
            ),
            (HostOs::MacOs, Starting, Some(DockerDesktop), Coming, None),
            (
                HostOs::MacOs,
                Ready,
                Some(DockerDesktop),
                AlreadyRunning,
                None,
            ),
            (
                HostOs::Linux,
                NotRunning,
                Some(DockerEngine),
                Unsupported,
                None,
            ),
            (HostOs::Linux, NotRunning, None, Unsupported, None),
        ];
        for (os, state, flavor, want, launched) in cases {
            let launcher = FakeLauncher::new();
            let answer = start_docker(&launcher, os, &status(state, flavor))
                .await
                .unwrap();
            assert_eq!(answer.result, want, "{os:?} {state:?} {flavor:?}");
            let want_launched: Vec<Vec<String>> = launched
                .map(|command| command.argv.iter().map(|&a| a.to_owned()).collect())
                .into_iter()
                .collect();
            assert_eq!(launcher.launched(), want_launched, "{os:?} {state:?}");
        }
    }

    #[tokio::test]
    async fn docker_engine_gets_the_systemctl_hint() {
        let answer = start_docker(
            &FakeLauncher::new(),
            HostOs::Linux,
            &status(EngineState::NotRunning, Some(EngineFlavor::DockerEngine)),
        )
        .await
        .unwrap();
        assert_eq!(
            answer.command.as_deref(),
            Some("sudo systemctl start docker")
        );
    }

    #[tokio::test]
    async fn a_missing_desktop_is_an_error() {
        let error = start_docker(
            &FakeLauncher::not_installed(),
            HostOs::MacOs,
            &status(EngineState::NotRunning, Some(EngineFlavor::DockerDesktop)),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, LaunchError::NotInstalled), "{error:?}");
    }

    #[test]
    fn runtime_dir_falls_back_to_systemds() {
        let cases = [
            (Some("/run/user/7"), "/run/user/7"),
            (Some(""), "/run/user/1000"),
            (None, "/run/user/1000"),
        ];
        for (own, want) in cases {
            assert_eq!(runtime_dir(own.map(OsString::from), 1000), want, "{own:?}");
        }
    }

    #[tokio::test]
    async fn the_system_launcher_reads_the_exit_status() {
        let cases: [(&'static [&'static str], &str); 4] = [
            (&["/bin/sh", "-c", "exit 0"], "ok"),
            (&["/bin/sh", "-c", "exit 5"], "not installed"),
            (&["/bin/sh", "-c", "echo nope >&2; exit 3"], "nope"),
            (&["/nonexistent/alchemy-test-launcher"], "spawn"),
        ];
        for (argv, want) in cases {
            let result = SystemLauncher
                .launch(DesktopCommand {
                    argv,
                    not_installed: 5,
                })
                .await;
            let got = match result {
                Ok(()) => "ok".to_owned(),
                Err(LaunchError::NotInstalled) => "not installed".to_owned(),
                Err(LaunchError::Failed { detail, .. }) => detail,
                Err(LaunchError::Spawn { .. }) => "spawn".to_owned(),
                Err(error) => error.to_string(),
            };
            assert_eq!(got, want, "{argv:?}");
        }
    }
}
