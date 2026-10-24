//! The task that keeps the engine and container parts of [`State`]
//! current.

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use alchemy_core::engine::{
    ContainerEvent, EngineFlavor, EngineProbe, EngineState, EngineStatus, SocketProbe, classify,
    not_ready_flavor, reconnect_delay,
};
use futures_util::StreamExt as _;
use tokio::sync::watch;

use super::{
    ApiError, BollardApi, DockerApi, DockerCli, Endpoint, EndpointError, HostEnv, HostProbe, Os,
    ProcessDockerCli, SystemProbe, desktop_install_paths, desktop_installed, discover_endpoint,
    docker_binary_candidates, find_docker_binary, probe_socket,
};
use crate::State;

/// How often to probe an engine that is not ready.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Stopped containers inspected at once after a listing.
const INSPECT_CONCURRENCY: usize = 4;

/// Something that keeps the engine and container parts of [`State`]
/// current until shutdown.
///
/// [`EngineMonitor`] is the real one. Tests that only need the daemon's
/// front doors can implement this to publish a fixed status instead.
pub trait EngineSource: Send + 'static {
    /// Run until `shutdown` flips to `true` or its sender is dropped.
    fn run(self, state: State, shutdown: watch::Receiver<bool>) -> impl Future<Output = ()> + Send;
}

/// Builds the clients [`EngineMonitor`] talks to Docker through.
pub trait Connector {
    /// The `docker` CLI client.
    type Cli: DockerCli + Send + Sync;
    /// The Engine API client.
    type Api: DockerApi + Send + Sync;

    /// A client for the `docker` binary at `binary`.
    fn cli(&self, binary: PathBuf) -> Self::Cli;

    /// A client for the engine at `endpoint`.
    fn api(&self, endpoint: &Endpoint) -> Result<Self::Api, ApiError>;
}

/// [`Connector`] for the real `docker` binary and engine socket.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemConnector;

impl Connector for SystemConnector {
    type Cli = ProcessDockerCli;
    type Api = BollardApi;

    fn cli(&self, binary: PathBuf) -> ProcessDockerCli {
        ProcessDockerCli::new(binary)
    }

    fn api(&self, endpoint: &Endpoint) -> Result<BollardApi, ApiError> {
        BollardApi::connect(endpoint, BollardApi::DEFAULT_TIMEOUT)
    }
}

/// Probes the engine, publishes its [`EngineStatus`], and while it is
/// ready mirrors its containers from the event stream.
///
/// Each cycle rediscovers the endpoint and the `docker` binary, so
/// starting Docker, switching contexts, or installing it later is picked
/// up without restarting the daemon. An engine that is not ready is
/// probed every [`POLL_INTERVAL`]. When a ready engine's event stream ends,
/// the monitor waits [`reconnect_delay`] and probes again; that delay grows
/// while streams keep ending before delivering an event.
#[derive(Debug, Clone)]
pub struct EngineMonitor<P, C> {
    env: HostEnv,
    os: Os,
    probe: P,
    connector: C,
}

impl EngineMonitor<SystemProbe, SystemConnector> {
    /// A monitor for this host, reading the process environment once.
    pub fn system() -> Self {
        Self::new(
            HostEnv::current(),
            Os::current(),
            SystemProbe,
            SystemConnector,
        )
    }
}

impl<P, C> EngineMonitor<P, C>
where
    P: HostProbe + Send + Sync + 'static,
    C: Connector + Send + Sync + 'static,
{
    /// A monitor over the given environment and effects.
    pub fn new(env: HostEnv, os: Os, probe: P, connector: C) -> Self {
        Self {
            env,
            os,
            probe,
            connector,
        }
    }

    async fn cycle(&self, state: &State, last_ready: &mut Option<LastReady>) -> Outcome {
        let home = self.env.home.as_deref();
        let binaries = docker_binary_candidates(self.env.path.as_deref(), home, self.os);
        let cli = find_docker_binary(&self.probe, &binaries).map(|b| self.connector.cli(b));
        let installed = Installed {
            cli: cli.is_some(),
            desktop: desktop_installed(&self.probe, &desktop_install_paths(home, self.os)),
        };

        let endpoint = match discover_endpoint(&self.env, cli.as_ref(), &self.probe, self.os).await
        {
            Ok(endpoint) => endpoint,
            Err(error) => {
                tracing::debug!(error = %error, "no usable Docker endpoint");
                let status = unusable_endpoint_status(&error, installed);
                publish_not_ready(
                    state,
                    with_not_ready_flavor(status, installed, last_ready.as_ref()),
                );
                return Outcome::NotReady;
            }
        };

        let api = match self.connector.api(&endpoint) {
            Ok(api) => Some(api),
            Err(error) => {
                tracing::debug!(error = %error, endpoint = %endpoint, "no Docker engine client");
                None
            }
        };
        let socket = match &api {
            Some(api) => probe_socket(&self.probe, &endpoint, api).await,
            None => SocketProbe::Missing,
        };
        let mut status = EngineStatus::new(classify(&installed.probe(socket)));
        status.endpoint = Some(endpoint.to_string());
        let (Some(api), EngineState::Ready) = (api, status.state) else {
            publish_not_ready(
                state,
                with_not_ready_flavor(status, installed, last_ready.as_ref()),
            );
            return Outcome::NotReady;
        };

        match api.version().await {
            Ok(version) => version.apply_to(&mut status),
            Err(error) => tracing::debug!(error = %error, "Docker engine version unavailable"),
        }
        if let Some(cli) = &cli {
            match cli.compose_version().await {
                Ok(version) => status.compose_version = Some(version),
                Err(error) => tracing::debug!(error = %error, "Docker Compose version unavailable"),
            }
        }
        if let Some(flavor) = status.flavor {
            *last_ready = Some(LastReady {
                endpoint: endpoint.to_string(),
                flavor,
            });
        }
        publish_engine(state, status);
        follow(&api, state).await
    }
}

impl<P, C> EngineSource for EngineMonitor<P, C>
where
    P: HostProbe + Send + Sync + 'static,
    C: Connector + Send + Sync + 'static,
{
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        let mut backoff = Backoff::default();
        let mut last_ready = None;
        loop {
            let outcome = tokio::select! {
                biased;
                _ = shutdown.wait_for(|stop| *stop) => return,
                outcome = self.cycle(&state, &mut last_ready) => outcome,
            };
            let delay = backoff.next(outcome);
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stop| *stop) => return,
                () = tokio::time::sleep(delay) => {}
            }
        }
    }
}

/// Subscribe to events, then list and replace every container, then apply
/// events until the stream ends.
///
/// The event stream is polled before the listing so its request goes out
/// first; events that arrive during the listing are buffered and applied
/// on top of it, so nothing between the two is lost.
///
/// A listing says a container stopped but not how. Compose containers
/// stopped with no exit known yet are inspected alongside the events, a
/// few at a time, so each connection inspects a container at most once.
async fn follow(api: &impl DockerApi, state: &State) -> Outcome {
    let mut events = api.events();
    let mut pending: Vec<ContainerEvent> = Vec::new();
    let listing = {
        let list = api.list_containers();
        tokio::pin!(list);
        loop {
            tokio::select! {
                biased;
                item = events.next() => match item {
                    Some(Ok(event)) => pending.push(event),
                    Some(Err(error)) => {
                        tracing::debug!(error = %error, "Docker event stream failed");
                        return Outcome::Disconnected { delivered: false };
                    }
                    None => return Outcome::Disconnected { delivered: false },
                },
                listing = &mut list => break listing,
            }
        }
    };
    let listed = match listing {
        Ok(listed) => listed,
        Err(error) => {
            tracing::debug!(error = %error, "listing Docker containers failed");
            return Outcome::Disconnected { delivered: false };
        }
    };
    let mut delivered = !pending.is_empty();
    state.update_containers(|containers| {
        pending
            .iter()
            .fold(containers.replace_all(listed), |changed, event| {
                containers.apply(event) | changed
            })
    });

    let mut inspections = futures_util::stream::iter(state.containers().needs_inspect())
        .map(|id| async move {
            let result = api.inspect_exit(id.clone()).await;
            (id, result)
        })
        .buffer_unordered(INSPECT_CONCURRENCY);
    loop {
        tokio::select! {
            biased;
            item = events.next() => match item {
                Some(Ok(event)) => {
                    delivered = true;
                    state.update_containers(|containers| containers.apply(&event));
                }
                Some(Err(error)) => {
                    tracing::debug!(error = %error, "Docker event stream failed");
                    return Outcome::Disconnected { delivered };
                }
                None => {
                    tracing::debug!("Docker event stream ended");
                    return Outcome::Disconnected { delivered };
                }
            },
            Some((id, result)) = inspections.next() => match result {
                Ok(Some(exit)) => {
                    state.update_containers(|containers| containers.record_inspect(&id, exit));
                }
                Ok(None) => {}
                Err(error) => tracing::debug!(error = %error, id, "inspecting a stopped container failed"),
            },
        }
    }
}

fn publish_engine(state: &State, status: EngineStatus) {
    let summary = (status.state, status.endpoint.clone());
    if state.set_engine(status) {
        tracing::info!(state = ?summary.0, endpoint = ?summary.1, "Docker engine state changed");
    }
}

/// Containers are only known while the engine is ready.
fn publish_not_ready(state: &State, status: EngineStatus) {
    state.update_containers(|containers| containers.replace_all([]));
    publish_engine(state, status);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Installed {
    cli: bool,
    desktop: bool,
}

impl Installed {
    const fn probe(self, socket: SocketProbe) -> EngineProbe {
        EngineProbe {
            socket,
            cli_installed: self.cli,
            desktop_installed: self.desktop,
        }
    }
}

/// The flavor an endpoint reported the last time it was ready.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LastReady {
    endpoint: String,
    flavor: EngineFlavor,
}

/// `status` with the flavor [`not_ready_flavor`] infers, using `last_ready`
/// only when it was seen at the same endpoint.
fn with_not_ready_flavor(
    mut status: EngineStatus,
    installed: Installed,
    last_ready: Option<&LastReady>,
) -> EngineStatus {
    let last_known = last_ready
        .filter(|last| status.endpoint.as_deref() == Some(last.endpoint.as_str()))
        .map(|last| last.flavor);
    status.flavor = not_ready_flavor(status.state, installed.desktop, last_known);
    status
}

/// The status for an endpoint discovery rejected: `Unsupported` for a
/// remote transport, otherwise whatever a missing socket classifies as.
fn unusable_endpoint_status(error: &EndpointError, installed: Installed) -> EngineStatus {
    let (state, endpoint) = match error {
        EndpointError::Unsupported { endpoint } => (EngineState::Unsupported, endpoint),
        EndpointError::Invalid { endpoint } => {
            (classify(&installed.probe(SocketProbe::Missing)), endpoint)
        }
    };
    let mut status = EngineStatus::new(state);
    status.endpoint = Some(endpoint.clone());
    status
}

/// How a monitor cycle ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The engine is not ready, or its endpoint is unusable.
    NotReady,
    /// The engine was ready and its event stream ended. `delivered` is
    /// whether the stream produced at least one event first.
    Disconnected { delivered: bool },
}

/// Delay before the next cycle. Reconnect attempts count up while event
/// streams keep ending without delivering anything, and reset otherwise.
#[derive(Debug, Default)]
struct Backoff {
    attempt: u32,
}

impl Backoff {
    fn next(&mut self, outcome: Outcome) -> Duration {
        match outcome {
            Outcome::NotReady => {
                self.attempt = 0;
                POLL_INTERVAL
            }
            Outcome::Disconnected { delivered } => {
                if delivered {
                    self.attempt = 0;
                }
                let delay = reconnect_delay(self.attempt);
                self.attempt = self.attempt.saturating_add(1);
                delay
            }
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
    fn backoff_grows_while_streams_deliver_nothing() {
        let dropped = Outcome::Disconnected { delivered: false };
        let worked = Outcome::Disconnected { delivered: true };
        let cases = [
            (dropped, 1),
            (dropped, 2),
            (dropped, 4),
            (worked, 1),
            (dropped, 2),
            (Outcome::NotReady, 2),
            (dropped, 1),
            (dropped, 2),
            (dropped, 4),
            (dropped, 8),
            (dropped, 10),
            (dropped, 10),
        ];
        let mut backoff = Backoff::default();
        for (i, (outcome, secs)) in cases.into_iter().enumerate() {
            assert_eq!(
                backoff.next(outcome),
                Duration::from_secs(secs),
                "step {i}: {outcome:?}"
            );
        }
    }

    #[test]
    fn not_ready_flavor_uses_the_last_ready_flavor_of_the_same_endpoint() {
        let here = "unix:///var/run/docker.sock";
        let elsewhere = "unix:///home/me/.colima/docker.sock";
        let engine = LastReady {
            endpoint: here.into(),
            flavor: EngineFlavor::DockerEngine,
        };
        let cli = Installed {
            cli: true,
            desktop: false,
        };
        let desktop = Installed {
            cli: true,
            desktop: true,
        };
        let cases = [
            (here, cli, None, None),
            (here, desktop, None, Some(EngineFlavor::DockerDesktop)),
            (here, cli, Some(&engine), Some(EngineFlavor::DockerEngine)),
            (
                here,
                desktop,
                Some(&engine),
                Some(EngineFlavor::DockerEngine),
            ),
            (elsewhere, cli, Some(&engine), None),
            (
                elsewhere,
                desktop,
                Some(&engine),
                Some(EngineFlavor::DockerDesktop),
            ),
        ];
        for (endpoint, installed, last_ready, want) in cases {
            let mut status = EngineStatus::new(EngineState::NotRunning);
            status.endpoint = Some(endpoint.into());
            let got = with_not_ready_flavor(status, installed, last_ready);
            assert_eq!(got.flavor, want, "{endpoint} {installed:?} {last_ready:?}");
        }
    }

    #[test]
    fn unusable_endpoint_status_reports_the_configured_endpoint() {
        let tcp = "tcp://nas:2375";
        let relative = "unix://docker.sock";
        let none = Installed {
            cli: false,
            desktop: false,
        };
        let cli = Installed {
            cli: true,
            desktop: false,
        };
        let cases = [
            (
                EndpointError::Unsupported {
                    endpoint: tcp.into(),
                },
                none,
                EngineState::Unsupported,
                tcp,
            ),
            (
                EndpointError::Unsupported {
                    endpoint: tcp.into(),
                },
                cli,
                EngineState::Unsupported,
                tcp,
            ),
            (
                EndpointError::Invalid {
                    endpoint: relative.into(),
                },
                cli,
                EngineState::NotRunning,
                relative,
            ),
            (
                EndpointError::Invalid {
                    endpoint: relative.into(),
                },
                none,
                EngineState::NotInstalled,
                relative,
            ),
        ];
        for (error, installed, state, endpoint) in cases {
            let got = unusable_endpoint_status(&error, installed);
            let mut want = EngineStatus::new(state);
            want.endpoint = Some(endpoint.to_owned());
            assert_eq!(got, want, "{error:?} {installed:?}");
        }
    }
}
