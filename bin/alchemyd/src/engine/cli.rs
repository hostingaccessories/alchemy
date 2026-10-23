//! The `docker` CLI, for the few answers the Engine API cannot give.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::process::Command;

/// Questions only the `docker` binary can answer.
pub trait DockerCli {
    /// The current context's engine host, such as
    /// `unix:///Users/me/.docker/run/docker.sock`.
    fn context_host(&self) -> impl Future<Output = Result<String, CliError>> + Send;

    /// The Docker Compose plugin version, such as `2.29.7`.
    fn compose_version(&self) -> impl Future<Output = Result<String, CliError>> + Send;
}

/// Why a `docker` invocation gave no answer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CliError {
    /// The binary could not be started.
    #[error("failed to run {}", binary.display())]
    Spawn {
        /// The binary that was run.
        binary: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The command did not finish in time and was killed.
    #[error("docker did not answer within {after:?}")]
    Timeout {
        /// How long the command was given.
        after: Duration,
    },
    /// The command exited unsuccessfully.
    #[error("docker exited with {}: {stderr}", code.map_or_else(|| "a signal".to_owned(), |c| format!("status {c}")))]
    Failed {
        /// The exit code, or `None` if a signal ended it.
        code: Option<i32>,
        /// The start of its standard error.
        stderr: String,
    },
    /// The command succeeded but printed something unrecognised.
    #[error("unexpected docker output {output:?}")]
    UnexpectedOutput {
        /// The start of its standard output.
        output: String,
    },
}

const CONTEXT_HOST_ARGS: [&str; 4] = [
    "context",
    "inspect",
    "--format",
    "{{.Endpoints.docker.Host}}",
];
const COMPOSE_VERSION_ARGS: [&str; 3] = ["compose", "version", "--short"];
const MAX_CAPTURED: usize = 512;

/// Parse `docker context inspect --format {{.Endpoints.docker.Host}}`.
///
/// Go templates print `<no value>` for a missing field.
pub fn parse_context_host(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .filter(|line| *line != "<no value>")
        .map(str::to_owned)
}

/// Parse `docker compose version --short`, which prints `2.29.7` on
/// current releases and `v2.29.7` on some older ones.
pub fn parse_compose_version(stdout: &str) -> Option<String> {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let version = line.strip_prefix('v').unwrap_or(line);
    let well_formed = version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    well_formed.then(|| version.to_owned())
}

fn truncate(text: &str) -> String {
    let text = text.trim();
    let end = text
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|end| *end <= MAX_CAPTURED)
        .last()
        .unwrap_or(0);
    text.get(..end).unwrap_or_default().to_owned()
}

/// [`DockerCli`] that runs a `docker` binary directly.
///
/// The binary is an absolute path, arguments are a fixed argv list, and no
/// shell is involved. Each run has a timeout, after which the child is
/// killed.
#[derive(Debug, Clone)]
pub struct ProcessDockerCli {
    binary: PathBuf,
    timeout: Duration,
}

impl ProcessDockerCli {
    /// How long a `docker` command may run by default.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Run the binary at `binary` with [`Self::DEFAULT_TIMEOUT`].
    ///
    /// Find `binary` with [`find_docker_binary`](super::find_docker_binary).
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            timeout: Self::DEFAULT_TIMEOUT,
        }
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

    async fn run(&self, args: &[&str]) -> Result<String, CliError> {
        let output = Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(self.timeout, output)
            .await
            .map_err(|_| CliError::Timeout {
                after: self.timeout,
            })?
            .map_err(|source| CliError::Spawn {
                binary: self.binary.clone(),
                source,
            })?;
        check_status(output.status, &output.stderr)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn check_status(status: ExitStatus, stderr: &[u8]) -> Result<(), CliError> {
    if status.success() {
        Ok(())
    } else {
        Err(CliError::Failed {
            code: status.code(),
            stderr: truncate(&String::from_utf8_lossy(stderr)),
        })
    }
}

fn unexpected(stdout: &str) -> CliError {
    CliError::UnexpectedOutput {
        output: truncate(stdout),
    }
}

impl DockerCli for ProcessDockerCli {
    async fn context_host(&self) -> Result<String, CliError> {
        let stdout = self.run(&CONTEXT_HOST_ARGS).await?;
        parse_context_host(&stdout).ok_or_else(|| unexpected(&stdout))
    }

    async fn compose_version(&self) -> Result<String, CliError> {
        let stdout = self.run(&COMPOSE_VERSION_ARGS).await?;
        parse_compose_version(&stdout).ok_or_else(|| unexpected(&stdout))
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
    fn parse_context_host_takes_first_line() {
        let cases = [
            (
                "unix:///Users/me/.docker/run/docker.sock\n",
                Some("unix:///Users/me/.docker/run/docker.sock"),
            ),
            ("\n  tcp://nas:2375  \n", Some("tcp://nas:2375")),
            ("<no value>\n", None),
            ("", None),
            ("   \n\n", None),
        ];
        for (stdout, want) in cases {
            assert_eq!(
                parse_context_host(stdout),
                want.map(str::to_owned),
                "{stdout:?}"
            );
        }
    }

    #[test]
    fn parse_compose_version_accepts_short_output() {
        let cases = [
            ("2.29.7\n", Some("2.29.7")),
            ("v2.20.2\n", Some("2.20.2")),
            ("2.30.0-desktop.1\n", Some("2.30.0-desktop.1")),
            ("\n2.29.7\n", Some("2.29.7")),
            ("Docker Compose version v2.29.7\n", None),
            ("docker: 'compose' is not a docker command.", None),
            ("v", None),
            ("", None),
        ];
        for (stdout, want) in cases {
            assert_eq!(
                parse_compose_version(stdout),
                want.map(str::to_owned),
                "{stdout:?}"
            );
        }
    }

    #[test]
    fn truncate_caps_on_a_char_boundary() {
        let cases = [
            ("  short \n".to_owned(), "short".to_owned()),
            ("x".repeat(MAX_CAPTURED + 10), "x".repeat(MAX_CAPTURED)),
            (
                format!("{}é", "x".repeat(MAX_CAPTURED - 1)),
                "x".repeat(MAX_CAPTURED - 1),
            ),
            (String::new(), String::new()),
        ];
        for (text, want) in cases {
            assert_eq!(truncate(&text), want);
        }
    }
}
