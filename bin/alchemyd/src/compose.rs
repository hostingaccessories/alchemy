//! Docker Compose lifecycle actions, driven through the `docker` CLI.
//!
//! What to run is decided by pure helpers: [`compose_args`] builds the argv
//! and [`child_env`] the child's whole environment. Running it sits behind
//! [`ComposeRunner`], implemented by [`CliComposeRunner`].
//!
//! This lives in the daemon rather than `alchemy-core` because the argv is
//! a detail of the `docker` binary, not of the domain.

mod env;

pub use env::{ALLOWED_ENV, ALLOWED_ENV_PREFIXES, FIXED_ENV, child_env, is_allowed};

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use alchemy_core::StackName;
use tokio::process::Command;

use crate::engine::Endpoint;

/// A lifecycle action on a Compose project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ComposeAction {
    /// Create and start every service, detached.
    Up,
    /// Stop and remove containers and networks. Volumes are kept.
    Down,
    /// Start existing containers.
    Start,
    /// Stop running containers without removing them.
    Stop,
    /// Restart every service.
    Restart,
    /// Pull every service's image.
    Pull,
}

impl ComposeAction {
    /// The `docker compose` subcommand and its flags.
    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Up => &["up", "--detach"],
            Self::Down => &["down"],
            Self::Start => &["start"],
            Self::Stop => &["stop"],
            Self::Restart => &["restart"],
            Self::Pull => &["pull"],
        }
    }

    /// The subcommand name, such as `up`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Pull => "pull",
        }
    }
}

impl fmt::Display for ComposeAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Compose project on disk: its directory and its project name.
///
/// The directory is absolute and free of `.` and `..` components, so the
/// child's working directory never changes what it refers to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComposeProject {
    dir: PathBuf,
    name: StackName,
}

/// Why a [`ComposeProject`] directory was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ComposeProjectError {
    /// The directory was not an absolute path.
    #[error("compose project directory {} is not absolute", dir.display())]
    NotAbsolute {
        /// The rejected directory.
        dir: PathBuf,
    },
    /// The directory contained a `.` or `..` component.
    #[error("compose project directory {} is not normalized", dir.display())]
    NotNormalized {
        /// The rejected directory.
        dir: PathBuf,
    },
}

impl ComposeProject {
    /// A project named `name` rooted at `dir`.
    ///
    /// Only the path's shape is checked; whether it exists is up to Compose.
    pub fn new(dir: impl Into<PathBuf>, name: StackName) -> Result<Self, ComposeProjectError> {
        let dir = dir.into();
        if !dir.is_absolute() {
            return Err(ComposeProjectError::NotAbsolute { dir });
        }
        if has_dot_segment(&dir) {
            return Err(ComposeProjectError::NotNormalized { dir });
        }
        Ok(Self { dir, name })
    }

    /// The project directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The Compose project name.
    pub fn name(&self) -> &StackName {
        &self.name
    }
}

/// Whether any `/`-separated segment of `dir` is `.` or `..`.
///
/// Checked on the raw bytes because [`Path::components`] silently drops
/// interior `.` segments.
fn has_dot_segment(dir: &Path) -> bool {
    dir.as_os_str()
        .as_encoded_bytes()
        .split(|b| *b == b'/')
        .any(|segment| segment == b"." || segment == b"..")
}

/// The argv after the `docker` binary for `action` on `project`, such as
/// `compose --project-directory /srv/blog -p blog up --detach`.
pub fn compose_args(project: &ComposeProject, action: ComposeAction) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "compose".into(),
        "--project-directory".into(),
        project.dir.as_os_str().to_owned(),
        "-p".into(),
        project.name.as_str().into(),
    ];
    args.extend(action.args().iter().map(OsString::from));
    args
}

const MAX_CAPTURED: usize = 4096;

/// The last [`MAX_CAPTURED`] bytes of `text`, trimmed, on a character
/// boundary. Compose prints the reason for a failure last.
fn tail(text: &str) -> String {
    let text = text.trim();
    let start = text
        .char_indices()
        .map(|(i, _)| i)
        .find(|i| text.len() - i <= MAX_CAPTURED)
        .unwrap_or(text.len());
    text.get(start..).unwrap_or_default().to_owned()
}

/// Runs Compose lifecycle actions.
pub trait ComposeRunner {
    /// Run `action` on `project` and wait for it to finish.
    fn run(
        &self,
        project: &ComposeProject,
        action: ComposeAction,
    ) -> impl Future<Output = Result<(), ComposeError>> + Send;
}

/// Why a Compose action did not complete.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ComposeError {
    /// The `docker` binary path was not absolute.
    #[error("docker binary {} is not absolute", binary.display())]
    RelativeBinary {
        /// The rejected path.
        binary: PathBuf,
    },
    /// The binary could not be started.
    #[error("failed to run {}", binary.display())]
    Spawn {
        /// The binary that was run.
        binary: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The action did not finish in time and was killed.
    #[error("docker compose {action} did not finish within {after:?}")]
    Timeout {
        /// The action that was run.
        action: ComposeAction,
        /// How long it was given.
        after: Duration,
    },
    /// The action exited unsuccessfully.
    #[error("docker compose {action} exited with {}: {stderr}", code.map_or_else(|| "a signal".to_owned(), |c| format!("status {c}")))]
    Failed {
        /// The action that was run.
        action: ComposeAction,
        /// The exit code, or `None` if a signal ended it.
        code: Option<i32>,
        /// The end of its standard error.
        stderr: String,
    },
}

/// [`ComposeRunner`] that runs `docker compose` as a child process.
///
/// The binary is an absolute path, arguments come from [`compose_args`],
/// and no shell is involved. The environment is exactly [`child_env`] of
/// the daemon's, so `DOCKER_HOST` is the endpoint, and stdin is closed. Each
/// action has a timeout, after which the child is killed; dropping the
/// future kills it too.
#[derive(Debug, Clone)]
pub struct CliComposeRunner {
    binary: PathBuf,
    endpoint: Endpoint,
    env: BTreeMap<OsString, OsString>,
    timeout: Duration,
}

impl CliComposeRunner {
    /// How long an action may run by default. Generous, because `up` and
    /// `pull` download images.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_mins(15);

    /// Run the binary at `binary` against `endpoint` with
    /// [`Self::DEFAULT_TIMEOUT`].
    ///
    /// Find `binary` with [`find_docker_binary`](crate::engine::find_docker_binary).
    pub fn new(binary: impl Into<PathBuf>, endpoint: Endpoint) -> Result<Self, ComposeError> {
        let binary = binary.into();
        if !binary.is_absolute() {
            return Err(ComposeError::RelativeBinary { binary });
        }
        let env = child_env(std::env::vars_os(), endpoint.docker_host());
        Ok(Self {
            binary,
            endpoint,
            env,
            timeout: Self::DEFAULT_TIMEOUT,
        })
    }

    /// Build the child's environment from `parent` instead of the daemon's
    /// own, which [`Self::new`] reads.
    #[must_use]
    pub fn with_parent_env<K, V>(mut self, parent: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env = child_env(parent, self.endpoint.docker_host());
        self
    }

    /// Use `timeout` instead of the default.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The binary this runs.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The engine this targets.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn command(&self, project: &ComposeProject, action: ComposeAction) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .args(compose_args(project, action))
            .env_clear()
            .envs(&self.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

impl ComposeRunner for CliComposeRunner {
    async fn run(
        &self,
        project: &ComposeProject,
        action: ComposeAction,
    ) -> Result<(), ComposeError> {
        let output = self.command(project, action).output();
        let output = tokio::time::timeout(self.timeout, output)
            .await
            .map_err(|_| ComposeError::Timeout {
                action,
                after: self.timeout,
            })?
            .map_err(|source| ComposeError::Spawn {
                binary: self.binary.clone(),
                source,
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(ComposeError::Failed {
                action,
                code: output.status.code(),
                stderr: tail(&String::from_utf8_lossy(&output.stderr)),
            })
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

    fn project(dir: &str, name: &str) -> ComposeProject {
        ComposeProject::new(dir, StackName::new(name).unwrap()).unwrap()
    }

    #[test]
    fn builds_argv_for_every_action() {
        let blog = project("/srv/stacks/blog", "blog");
        let prefix = "compose --project-directory /srv/stacks/blog -p blog";
        let cases = [
            (ComposeAction::Up, "up --detach"),
            (ComposeAction::Down, "down"),
            (ComposeAction::Start, "start"),
            (ComposeAction::Stop, "stop"),
            (ComposeAction::Restart, "restart"),
            (ComposeAction::Pull, "pull"),
        ];
        for (action, want) in cases {
            let args: Vec<String> = compose_args(&blog, action)
                .into_iter()
                .map(|a| a.into_string().unwrap())
                .collect();
            assert_eq!(args.join(" "), format!("{prefix} {want}"), "{action}");
            assert_eq!(args[5], action.as_str());
        }
    }

    #[test]
    fn keeps_a_directory_with_spaces_as_one_argument() {
        let args = compose_args(&project("/srv/my stacks/blog", "blog"), ComposeAction::Stop);
        assert_eq!(args[2], OsString::from("/srv/my stacks/blog"));
        assert_eq!(args.len(), 6);
    }

    #[test]
    fn validates_the_project_directory() {
        let name = StackName::new("blog").unwrap();
        let cases = [
            ("/srv/blog", None),
            ("/srv/blog/", None),
            ("/", None),
            (
                "srv/blog",
                Some(ComposeProjectError::NotAbsolute {
                    dir: "srv/blog".into(),
                }),
            ),
            (
                "",
                Some(ComposeProjectError::NotAbsolute { dir: "".into() }),
            ),
            (
                "/srv/../etc",
                Some(ComposeProjectError::NotNormalized {
                    dir: "/srv/../etc".into(),
                }),
            ),
            (
                "/srv/blog/.",
                Some(ComposeProjectError::NotNormalized {
                    dir: "/srv/blog/.".into(),
                }),
            ),
            ("/srv/.blog/..x", None),
            (
                "/srv/./blog",
                Some(ComposeProjectError::NotNormalized {
                    dir: "/srv/./blog".into(),
                }),
            ),
        ];
        for (dir, want) in cases {
            let got = ComposeProject::new(dir, name.clone());
            assert_eq!(got.as_ref().err(), want.as_ref(), "{dir:?}");
            if let Ok(project) = got {
                assert_eq!(project.dir(), Path::new(dir));
                assert_eq!(project.name(), &name);
            }
        }
    }

    #[test]
    fn rejects_a_relative_binary() {
        assert!(matches!(
            CliComposeRunner::new("docker", Endpoint::system()),
            Err(ComposeError::RelativeBinary { .. })
        ));
        let runner = CliComposeRunner::new("/usr/bin/docker", Endpoint::system()).unwrap();
        assert_eq!(runner.binary(), Path::new("/usr/bin/docker"));
        assert_eq!(runner.endpoint(), &Endpoint::system());
    }

    #[test]
    fn keeps_the_tail_of_long_output() {
        assert_eq!(tail("  boom\n"), "boom");
        assert_eq!(tail(""), "");
        let long = format!("{}end", "x".repeat(MAX_CAPTURED));
        let kept = tail(&long);
        assert_eq!(kept.len(), MAX_CAPTURED);
        assert!(kept.ends_with("end"));
        let wide = "é".repeat(MAX_CAPTURED);
        let kept = tail(&wide);
        assert!(kept.len() <= MAX_CAPTURED);
        assert!(kept.chars().all(|c| c == 'é'));
    }
}
