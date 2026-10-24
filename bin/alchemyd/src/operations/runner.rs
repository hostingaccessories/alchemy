//! [`OperationRunner`]: how the operations manager runs a plan, against
//! whichever engine and `docker` binary are current when it starts.

use std::fmt;
use std::path::PathBuf;

use futures_util::future::BoxFuture;
use tokio::sync::mpsc;

use crate::compose::{
    CancelToken, CliComposeRunner, ComposeError, ComposeEvent, ComposePlan, ComposeRunner,
    FakeComposeRunner,
};
use crate::engine::{
    Endpoint, HostEnv, Os, SystemProbe, docker_binary_candidates, find_docker_binary,
};

/// Runs one operation's plan.
///
/// The object-safe counterpart of [`ComposeRunner`], which also takes the
/// engine endpoint, because the engine an operation targets is the one
/// current when it starts rather than when the daemon started.
pub trait OperationRunner: Send + Sync + fmt::Debug {
    /// Run `plan` against `endpoint`, as [`ComposeRunner::run`] does.
    fn run<'a>(
        &'a self,
        plan: &'a ComposePlan,
        endpoint: Endpoint,
        events: mpsc::Sender<ComposeEvent>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Result<(), ComposeError>>;
}

/// [`OperationRunner`] that runs the real `docker` binary through
/// [`CliComposeRunner`].
///
/// It looks for `docker` where the engine monitor does on every run, so
/// installing Docker later is picked up without a restart.
#[derive(Debug, Clone)]
pub struct SystemRunner {
    candidates: Vec<PathBuf>,
}

impl SystemRunner {
    /// Look for `docker` where the engine monitor does, given this
    /// process's environment.
    pub fn new() -> Self {
        let env = HostEnv::current();
        Self {
            candidates: docker_binary_candidates(
                env.path.as_deref(),
                env.home.as_deref(),
                Os::current(),
            ),
        }
    }
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl OperationRunner for SystemRunner {
    fn run<'a>(
        &'a self,
        plan: &'a ComposePlan,
        endpoint: Endpoint,
        events: mpsc::Sender<ComposeEvent>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Result<(), ComposeError>> {
        Box::pin(async move {
            let Some(binary) = find_docker_binary(&SystemProbe, &self.candidates) else {
                return Err(ComposeError::Spawn {
                    binary: PathBuf::from("docker"),
                    source: std::io::ErrorKind::NotFound.into(),
                });
            };
            CliComposeRunner::new(binary, endpoint)?
                .run(plan, events, cancel)
                .await
        })
    }
}

impl OperationRunner for FakeComposeRunner {
    fn run<'a>(
        &'a self,
        plan: &'a ComposePlan,
        _endpoint: Endpoint,
        events: mpsc::Sender<ComposeEvent>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Result<(), ComposeError>> {
        Box::pin(ComposeRunner::run(self, plan, events, cancel))
    }
}
