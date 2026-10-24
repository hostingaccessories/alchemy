//! [`CliComposeRunner`]: a [`ComposePlan`] as `docker compose` child
//! processes.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::lines::{LineSplitter, MAX_LINE};
use super::runner::FailureNotes;
use super::{
    CancelToken, ComposeAction, ComposeError, ComposeEvent, ComposePlan, ComposeRunner,
    ComposeStep, OutputLine, Stream, child_env,
};
use crate::engine::Endpoint;

/// [`ComposeRunner`] that runs `docker compose` as child processes, one
/// per step.
///
/// The binary is an absolute path, arguments come from [`ComposeStep`],
/// and no shell is involved. The environment is exactly [`child_env`] of
/// the daemon's, so `DOCKER_HOST` is the endpoint, and stdin is closed.
///
/// Each child leads its own process group, so a Ctrl-C meant for the
/// daemon's terminal does not reach it; the daemon cancels runs itself.
/// Cancelling, or running past the timeout, sends `SIGINT` to the group,
/// as Ctrl-C in a terminal would, waits up to the grace period for Compose
/// to clean up, then sends `SIGKILL`. Dropping the future kills the child
/// at once.
#[derive(Debug, Clone)]
pub struct CliComposeRunner {
    binary: PathBuf,
    endpoint: Endpoint,
    env: BTreeMap<OsString, OsString>,
    timeout: Duration,
    grace: Duration,
}

/// How long to wait for output to drain after `SIGKILL`, in case something
/// outside the group still holds a pipe open.
const DRAIN_AFTER_KILL: Duration = Duration::from_secs(2);

const READ_BUFFER: usize = 8 * 1024;

/// Why a step was stopped before it exited on its own.
#[derive(Debug, Clone, Copy)]
enum Stop {
    Cancelled,
    TimedOut,
}

impl CliComposeRunner {
    /// How long a whole plan may run by default. Generous, because `up`,
    /// `pull`, and `build` download images.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_mins(15);

    /// How long Compose gets to stop cleanly after `SIGINT` by default.
    pub const DEFAULT_GRACE: Duration = Duration::from_secs(10);

    /// Run the binary at `binary` against `endpoint` with
    /// [`Self::DEFAULT_TIMEOUT`] and [`Self::DEFAULT_GRACE`], and the
    /// child's environment built from the daemon's.
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
            grace: Self::DEFAULT_GRACE,
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

    /// Use `timeout` for a whole plan instead of the default.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Use `grace` between `SIGINT` and `SIGKILL` instead of the default.
    #[must_use]
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
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

    /// The child's whole environment.
    pub fn env(&self) -> &BTreeMap<OsString, OsString> {
        &self.env
    }

    fn command(&self, step: &ComposeStep) -> Command {
        docker_command(&self.binary, step.args(), &self.env)
    }

    async fn run_step(
        &self,
        action: ComposeAction,
        index: usize,
        step: &ComposeStep,
        events: &mpsc::Sender<ComposeEvent>,
        cancel: &mut CancelToken,
        deadline: Instant,
    ) -> Result<(), ComposeError> {
        let mut child = self
            .command(step)
            .spawn()
            .map_err(|source| ComposeError::Spawn {
                binary: self.binary.clone(),
                source,
            })?;
        let group = process_group(&child);
        let stdout = pump(child.stdout.take(), Stream::Stdout, index, events);
        let stderr = pump(child.stderr.take(), Stream::Stderr, index, events);
        let finished = async { tokio::join!(stdout, stderr, child.wait()) };
        tokio::pin!(finished);

        let reason = tokio::select! {
            (_, notes, status) = &mut finished => {
                return check(action, step, status, notes);
            }
            () = cancel.cancelled() => Stop::Cancelled,
            () = tokio::time::sleep_until(deadline) => Stop::TimedOut,
        };

        signal(group, Signal::INT);
        if tokio::time::timeout(self.grace, &mut finished)
            .await
            .is_err()
        {
            signal(group, Signal::KILL);
            let _ = tokio::time::timeout(DRAIN_AFTER_KILL, &mut finished).await;
        }
        Err(match reason {
            Stop::Cancelled => ComposeError::Cancelled { action },
            Stop::TimedOut => ComposeError::Timeout {
                action,
                after: self.timeout,
            },
        })
    }
}

impl ComposeRunner for CliComposeRunner {
    async fn run(
        &self,
        plan: &ComposePlan,
        events: mpsc::Sender<ComposeEvent>,
        mut cancel: CancelToken,
    ) -> Result<(), ComposeError> {
        let action = plan.action();
        let deadline = Instant::now() + self.timeout;
        for (index, step) in plan.steps().iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(ComposeError::Cancelled { action });
            }
            let command = step.command();
            let _ = events
                .send(ComposeEvent::StepStarted {
                    step: index,
                    command,
                })
                .await;
            self.run_step(action, index, step, &events, &mut cancel, deadline)
                .await?;
            let _ = events
                .send(ComposeEvent::StepFinished {
                    step: index,
                    command,
                })
                .await;
        }
        Ok(())
    }
}

/// Read `reader` to its end, sending each line to `events`, and return
/// what its lines said about a failure.
///
/// A read error ends the stream like end of file: the exit status still
/// decides the outcome.
async fn pump<R: AsyncRead + Unpin>(
    reader: Option<R>,
    stream: Stream,
    step: usize,
    events: &mpsc::Sender<ComposeEvent>,
) -> FailureNotes {
    let mut notes = FailureNotes::default();
    let Some(mut reader) = reader else {
        return notes;
    };
    let mut splitter = LineSplitter::new(MAX_LINE);
    let mut buffer = vec![0; READ_BUFFER];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        for text in splitter.push(buffer.get(..read).unwrap_or_default()) {
            emit(OutputLine::new(step, stream, text), &mut notes, events).await;
        }
    }
    if let Some(text) = splitter.finish() {
        emit(OutputLine::new(step, stream, text), &mut notes, events).await;
    }
    notes
}

async fn emit(line: OutputLine, notes: &mut FailureNotes, events: &mpsc::Sender<ComposeEvent>) {
    if line.stream == Stream::Stderr {
        notes.note(&line);
    }
    let _ = events.send(ComposeEvent::Output(line)).await;
}

fn check(
    action: ComposeAction,
    step: &ComposeStep,
    status: std::io::Result<ExitStatus>,
    notes: FailureNotes,
) -> Result<(), ComposeError> {
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(ComposeError::Failed {
            action,
            command: step.command(),
            code: status.code(),
            message: notes.message(),
        }),
        Err(err) => Err(ComposeError::Failed {
            action,
            command: step.command(),
            code: None,
            message: format!("waiting for docker compose failed: {err}"),
        }),
    }
}

/// `binary` with `args` and exactly `env`, stdin closed, stdout and stderr
/// piped, leading its own process group, and killed if dropped. No shell
/// is involved.
pub(super) fn docker_command(
    binary: &Path,
    args: &[OsString],
    env: &BTreeMap<OsString, OsString>,
) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    command
}

/// The process group a spawned [`docker_command`] leads.
pub(super) fn process_group(child: &tokio::process::Child) -> Option<Pid> {
    child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
}

/// Send `signal` to the child's process group.
///
/// Only called while the child is unreaped, so its id, and therefore the
/// group's, cannot have been reused. A group that has already gone is not
/// an error.
pub(super) fn signal(group: Option<Pid>, signal: Signal) {
    if let Some(group) = group
        && let Err(err) = kill_process_group(group, signal)
    {
        tracing::debug!(%err, ?signal, "signalling docker compose failed");
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
    fn builds_the_environment_from_the_given_parent() {
        let runner = CliComposeRunner::new("/usr/bin/docker", Endpoint::system())
            .unwrap()
            .with_parent_env([("HOME", "/home/me"), ("SECRET", "x")]);
        assert_eq!(
            runner.env(),
            &child_env([("HOME", "/home/me")], "unix:///var/run/docker.sock")
        );
    }
}
