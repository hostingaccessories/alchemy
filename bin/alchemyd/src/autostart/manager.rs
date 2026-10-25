//! The [`ServiceManager`] trait for the per-user service, and a fake.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::future::{self, BoxFuture};

/// Installs and removes the per-user service: a launch agent in
/// `gui/<uid>` on macOS, a `systemd --user` unit on Linux.
///
/// Object safe, so the daemon can hold any implementation. Both changes
/// are idempotent and report whether they changed anything. The file's
/// contents come from the caller, rendered by
/// [`alchemy_core::lifecycle`].
pub trait ServiceManager: Send + Sync + fmt::Debug {
    /// The service file as it is on disk, or `None` if there is none.
    fn read(&self) -> BoxFuture<'_, Result<Option<String>, ServiceError>>;

    /// Whether there is a service manager to load it: always on macOS,
    /// and on Linux when the user's `systemd --user` runs.
    fn available(&self) -> BoxFuture<'_, bool>;

    /// Write `file`, unless it already holds exactly that, and register
    /// it so it starts at the next login. With `start`, also make sure it
    /// runs now, reloading it if the file changed. Without `start`, the
    /// running service is never touched, which is how the service updates
    /// its own file.
    fn install<'a>(
        &'a self,
        file: &'a str,
        start: bool,
    ) -> BoxFuture<'a, Result<bool, ServiceError>>;

    /// Unregister the service and delete its file. With `stop`, also stop
    /// it if it runs; without, a running service keeps running until it
    /// exits, which is how the service removes itself.
    fn remove(&self, stop: bool) -> BoxFuture<'_, Result<bool, ServiceError>>;
}

/// Why the per-user service could not be changed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServiceError {
    /// A file or directory could not be read, written, or removed.
    #[error("cannot {action} {}: {source}", .path.display())]
    File {
        /// What was being done, such as `write`.
        action: &'static str,
        /// The path.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// `launchctl` or `systemctl` could not be run.
    #[error("cannot run {program}: {source}")]
    Spawn {
        /// The program.
        program: &'static str,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// `launchctl` or `systemctl` failed.
    #[error("`{command}` failed ({}){}", exit(*.code), detail(.stderr))]
    Command {
        /// The command line.
        command: String,
        /// Its exit code, or `None` when a signal ended it.
        code: Option<i32>,
        /// The last line it wrote to standard error.
        stderr: String,
    },
    /// The old instance of the service did not go away in time.
    #[error("the old {what} did not stop in time")]
    Timeout {
        /// What was waited for.
        what: String,
    },
}

fn exit(code: Option<i32>) -> String {
    code.map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}"))
}

fn detail(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

/// One call made to a [`FakeServiceManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeCall {
    /// [`ServiceManager::install`].
    Install {
        /// The file written.
        file: String,
        /// Whether it was started.
        start: bool,
    },
    /// [`ServiceManager::remove`].
    Remove {
        /// Whether it was stopped.
        stop: bool,
    },
}

/// A [`ServiceManager`] for tests: an in-memory service file and a log of
/// every change. Cheap to clone: every clone shares the same state.
#[derive(Debug, Clone, Default)]
pub struct FakeServiceManager {
    inner: Arc<Mutex<FakeState>>,
}

#[derive(Debug, Default)]
struct FakeState {
    file: Option<String>,
    unavailable: bool,
    fail: Option<String>,
    calls: Vec<FakeCall>,
}

impl FakeServiceManager {
    /// No service installed, and a service manager to load one.
    pub fn new() -> Self {
        Self::default()
    }

    /// The service file now, if any.
    pub fn file(&self) -> Option<String> {
        self.with(|s| s.file.clone())
    }

    /// Put `file` on disk, as if installed by hand.
    pub fn set_file(&self, file: Option<&str>) {
        self.with(|s| s.file = file.map(str::to_owned));
    }

    /// Say there is no service manager to load a service.
    pub fn set_unavailable(&self) {
        self.with(|s| s.unavailable = true);
    }

    /// Fail every change from now on with a command failure reading
    /// `stderr`, or stop failing with `None`.
    pub fn fail(&self, stderr: Option<&str>) {
        self.with(|s| s.fail = stderr.map(str::to_owned));
    }

    /// Every change asked for so far.
    pub fn calls(&self) -> Vec<FakeCall> {
        self.with(|s| s.calls.clone())
    }

    fn with<T>(&self, f: impl FnOnce(&mut FakeState) -> T) -> T {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut state)
    }

    fn failure(stderr: String) -> ServiceError {
        ServiceError::Command {
            command: "fake".into(),
            code: Some(1),
            stderr,
        }
    }
}

impl ServiceManager for FakeServiceManager {
    fn read(&self) -> BoxFuture<'_, Result<Option<String>, ServiceError>> {
        Box::pin(future::ready(Ok(self.file())))
    }

    fn available(&self) -> BoxFuture<'_, bool> {
        Box::pin(future::ready(!self.with(|s| s.unavailable)))
    }

    fn install<'a>(
        &'a self,
        file: &'a str,
        start: bool,
    ) -> BoxFuture<'a, Result<bool, ServiceError>> {
        let result = self.with(|s| {
            s.calls.push(FakeCall::Install {
                file: file.to_owned(),
                start,
            });
            if let Some(stderr) = s.fail.clone() {
                return Err(Self::failure(stderr));
            }
            let changed = s.file.as_deref() != Some(file);
            s.file = Some(file.to_owned());
            Ok(changed)
        });
        Box::pin(future::ready(result))
    }

    fn remove(&self, stop: bool) -> BoxFuture<'_, Result<bool, ServiceError>> {
        let result = self.with(|s| {
            s.calls.push(FakeCall::Remove { stop });
            if let Some(stderr) = s.fail.clone() {
                return Err(Self::failure(stderr));
            }
            Ok(s.file.take().is_some())
        });
        Box::pin(future::ready(result))
    }
}
