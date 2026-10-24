//! Running a [`ComposePlan`] as `docker compose` child processes.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use super::{ComposeAction, ComposeCommand, ComposePlan, ComposeStep, child_env};
use crate::engine::Endpoint;

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
    /// Run every step of `plan` in order and wait for the last to finish.
    fn run(&self, plan: &ComposePlan) -> impl Future<Output = Result<(), ComposeError>> + Send;
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
    /// A step exited unsuccessfully, and the steps after it did not run.
    #[error("docker compose {command} exited with {}: {stderr}", code.map_or_else(|| "a signal".to_owned(), |c| format!("status {c}")))]
    Failed {
        /// The action that was run.
        action: ComposeAction,
        /// The step that failed.
        command: ComposeCommand,
        /// The exit code, or `None` if a signal ended it.
        code: Option<i32>,
        /// The end of its standard error.
        stderr: String,
    },
}

/// [`ComposeRunner`] that runs `docker compose` as a child process.
///
/// The binary is an absolute path, arguments come from [`ComposeStep`],
/// and no shell is involved. The environment is exactly [`child_env`] of
/// the daemon's, so `DOCKER_HOST` is the endpoint, and stdin is closed. Each
/// plan has a timeout, after which the child is killed; dropping the
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

    fn command(&self, step: &ComposeStep) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .args(step.args())
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
    async fn run(&self, plan: &ComposePlan) -> Result<(), ComposeError> {
        let action = plan.action();
        let deadline = tokio::time::Instant::now() + self.timeout;
        for step in plan.steps() {
            self.run_step(action, step, deadline).await?;
        }
        Ok(())
    }
}

impl CliComposeRunner {
    async fn run_step(
        &self,
        action: ComposeAction,
        step: &ComposeStep,
        deadline: tokio::time::Instant,
    ) -> Result<(), ComposeError> {
        let output = self.command(step).output();
        let output = tokio::time::timeout_at(deadline, output)
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
                command: step.command(),
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
