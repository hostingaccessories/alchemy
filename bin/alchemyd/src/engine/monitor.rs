//! The task that keeps the engine and container parts of [`State`]
//! current.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use alchemy_core::engine::{
    ContainerAction, ContainerEvent, ContainerState, ContainerStatus, Containers, EngineFlavor,
    EngineProbe, EngineState, EngineStatus, SocketProbe, classify, not_ready_flavor,
    reconnect_delay,
};
use alchemy_core::lifecycle::{Liveness, LivenessCheck, PingOutcome};
use futures_util::StreamExt as _;
use futures_util::future::{FusedFuture as _, FutureExt as _};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{
    ApiError, BollardApi, DockerApi, DockerCli, Endpoint, EndpointError, EngineInfo, HostEnv,
    HostProbe, Os, ProcessDockerCli, SystemProbe, desktop_install_paths, desktop_installed,
    discover_endpoint, docker_binary_candidates, engine_facts, find_docker_binary, probe_socket,
};
use crate::State;

/// How often to probe an engine that is not ready.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Stopped containers inspected at once after a listing.
const INSPECT_CONCURRENCY: usize = 4;

/// How many more times the containers are listed when a running
/// container's network `connect` or `disconnect` isn't in the listing
/// yet. Docker sends the event before its listing shows the change.
const NETWORK_RELISTS: u32 = 5;

/// The wait before the first of [`NETWORK_RELISTS`], doubled each time.
const NETWORK_RELIST_DELAY: Duration = Duration::from_millis(100);

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
///
/// A stream can also hang without ending, as one may after sleep. So a
/// stream that has been quiet for as long as its [`Liveness`] says is
/// checked with a ping, and one whose ping fails or takes too long counts
/// as ended. After a wake, from [`State::watch_wakes`], the monitor
/// subscribes again at once whatever a ping would say, since a ping on a
/// new connection proves nothing about the old stream.
#[derive(Debug, Clone)]
pub struct EngineMonitor<P, C> {
    env: HostEnv,
    os: Os,
    probe: P,
    connector: C,
    liveness: Liveness,
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
    /// A monitor over the given environment and effects, pinging a quiet
    /// stream as [`Liveness::default`] says.
    pub fn new(env: HostEnv, os: Os, probe: P, connector: C) -> Self {
        Self {
            env,
            os,
            probe,
            connector,
            liveness: Liveness::default(),
        }
    }

    /// Ping a quiet stream as `liveness` says instead.
    #[must_use]
    pub fn with_liveness(mut self, liveness: Liveness) -> Self {
        self.liveness = liveness;
        self
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

        let version = match api.version().await {
            Ok(version) => {
                version.apply_to(&mut status);
                Some(version)
            }
            Err(error) => {
                tracing::debug!(error = %error, "Docker engine version unavailable");
                None
            }
        };
        let info = api.info().await.unwrap_or_else(|error| {
            tracing::debug!(error = %error, "Docker engine info unavailable");
            EngineInfo::default()
        });
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
        state.set_engine_facts(Some(engine_facts(version.as_ref(), &info, self.os)));
        publish_engine(state, status);
        let outcome = follow(&api, state, self.liveness).await;
        state.engine_lost();
        outcome
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
/// on top of it, so nothing between the two is lost. Once they are in
/// [`State::containers`], [`State::engine_listed`] says so; the caller
/// marks the link lost when this returns.
///
/// A listing says a container stopped but not how. Compose containers
/// stopped with no exit known yet are inspected alongside the events, a
/// few at a time, so each connection inspects a container at most once.
///
/// Events carry no ports or addresses, so a `start`, `restart`, or network
/// `connect` or `disconnect` lists the containers again, one listing at a
/// time, and takes only their ports and addresses from it. A running
/// container whose addresses that listing doesn't change after a network
/// event is listed again up to [`NETWORK_RELISTS`] times, since the event
/// can come before the listing shows it.
///
/// Once following, a stream quiet for as long as `liveness` says gets a
/// ping, and one that fails ends it; a wake ends it at once.
async fn follow(api: &impl DockerApi, state: &State, liveness: Liveness) -> Outcome {
    let mut woke = state.watch_wakes();
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
    state.engine_listed();

    let mut inspections = futures_util::stream::iter(state.containers().needs_inspect())
        .map(|id| async move {
            let result = api.inspect_exit(id.clone()).await;
            (id, result)
        })
        .buffer_unordered(INSPECT_CONCURRENCY);
    let relist = futures_util::future::Fuse::terminated();
    tokio::pin!(relist);
    let mut relists = Relists::default();
    let mut heard = Instant::now();
    loop {
        if relist.is_terminated()
            && let Some(delay) = relists.start()
        {
            relist.set(list_after(api, delay).fuse());
        }
        let quiet = until_ping(liveness, heard, state);
        tokio::select! {
            biased;
            Ok(()) = woke.changed() => {
                tracing::info!("subscribing to Docker events again after a wake");
                return Outcome::Woke;
            }
            item = events.next() => match item {
                Some(Ok(event)) => {
                    delivered = true;
                    heard = Instant::now();
                    relists.event(&event);
                    state.update_containers(|containers| containers.apply(&event));
                }
                end => {
                    tracing::debug!(error = ?end.and_then(Result::err), "Docker event stream ended");
                    return Outcome::Disconnected { delivered };
                }
            },
            listing = &mut relist, if !relist.is_terminated() => match listing {
                Ok(listed) => {
                    let before = relists.addresses(&state.containers());
                    state.update_containers(|containers| containers.update_listed(listed));
                    relists.listed(&before, &state.containers());
                }
                Err(error) => tracing::debug!(error = %error, "listing Docker containers again failed"),
            },
            Some((id, result)) = inspections.next() => match result {
                Ok(Some(exit)) => {
                    state.update_containers(|containers| containers.record_inspect(&id, exit));
                }
                Ok(None) => {}
                Err(error) => tracing::debug!(error = %error, id, "inspecting a stopped container failed"),
            },
            () = tokio::time::sleep(quiet) => {
                if !answers(api, liveness).await {
                    return Outcome::Disconnected { delivered };
                }
                heard = Instant::now();
            }
        }
    }
}

/// How long until a stream last heard from at `heard` is due a ping. It
/// waits longer while no container is running.
fn until_ping(liveness: Liveness, heard: Instant, state: &State) -> Duration {
    let running = state
        .watch_containers()
        .borrow()
        .iter()
        .any(|c| c.state == ContainerState::Running);
    match liveness.check(heard.elapsed(), running) {
        LivenessCheck::Wait(left) => left,
        LivenessCheck::Ping => Duration::ZERO,
    }
}

/// Ping the engine, and say whether the connection is still alive as
/// `liveness` judges it.
async fn answers(api: &impl DockerApi, liveness: Liveness) -> bool {
    let started = Instant::now();
    let ping = match tokio::time::timeout(liveness.ping_timeout, api.ping()).await {
        Ok(Ok(())) => PingOutcome::Answered(started.elapsed()),
        Ok(Err(error)) => {
            tracing::debug!(error = %error, "pinging Docker failed");
            PingOutcome::Failed
        }
        Err(_) => PingOutcome::TimedOut,
    };
    let alive = liveness.alive(ping);
    if !alive {
        tracing::info!(
            ?ping,
            "Docker stopped answering on a quiet event stream; reconnecting"
        );
    }
    alive
}

async fn list_after(
    api: &impl DockerApi,
    delay: Duration,
) -> Result<Vec<ContainerStatus>, ApiError> {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    api.list_containers().await
}

type Addresses = BTreeMap<String, Ipv4Addr>;

/// When [`follow`] lists the containers again, and which network changes
/// it is still waiting to see.
#[derive(Debug, Default)]
struct Relists {
    due: bool,
    delay: Duration,
    retries: u32,
    joining: BTreeSet<String>,
    checking: BTreeSet<String>,
}

impl Relists {
    /// Note `event`: a start, restart, or network change makes a listing
    /// due at once.
    fn event(&mut self, event: &ContainerEvent) {
        let network = matches!(
            event.action,
            ContainerAction::NetworkConnect | ContainerAction::NetworkDisconnect
        );
        if network {
            self.joining.insert(event.id.clone());
            self.retries = 0;
        }
        if network
            || matches!(
                event.action,
                ContainerAction::Start | ContainerAction::Restart
            )
        {
            self.due = true;
            self.delay = Duration::ZERO;
        }
    }

    /// The wait before the next listing, when one is due.
    fn start(&mut self) -> Option<Duration> {
        if !self.due {
            return None;
        }
        self.due = false;
        self.checking = std::mem::take(&mut self.joining);
        Some(self.delay)
    }

    /// The addresses of each running container the listing in progress
    /// should change.
    fn addresses(&self, containers: &Containers) -> Vec<(String, Addresses)> {
        self.checking
            .iter()
            .filter_map(|id| containers.get(id))
            .filter(|c| c.state == ContainerState::Running)
            .map(|c| (c.id.clone(), c.networks.clone()))
            .collect()
    }

    /// After a listing: if a container in `before` is still running with
    /// the same addresses, list again after a growing wait, up to
    /// [`NETWORK_RELISTS`] times.
    fn listed(&mut self, before: &[(String, Addresses)], containers: &Containers) {
        let unchanged: Vec<String> = before
            .iter()
            .filter(|(id, addresses)| {
                containers
                    .get(id)
                    .is_some_and(|c| c.state == ContainerState::Running && c.networks == *addresses)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if unchanged.is_empty() || self.retries >= NETWORK_RELISTS {
            return;
        }
        self.delay = NETWORK_RELIST_DELAY.saturating_mul(1 << self.retries);
        self.retries += 1;
        self.joining.extend(unchanged);
        self.due = true;
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
    state.engine_lost();
    state.update_containers(|containers| containers.replace_all([]));
    state.set_engine_facts(None);
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
    /// The computer woke while following, so subscribe again at once.
    Woke,
}

/// Delay before the next cycle. Reconnect attempts count up while event
/// streams keep ending without delivering anything, and reset otherwise.
/// A wake reconnects at once.
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
            Outcome::Woke => {
                self.attempt = 0;
                Duration::ZERO
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

    fn containers(list: &[(&str, ContainerState, Option<&str>)]) -> Containers {
        let mut containers = Containers::new();
        containers.replace_all(list.iter().map(|&(id, state, ip)| {
            let mut c = ContainerStatus::new(id, id, state);
            if let Some(ip) = ip {
                c.networks.insert("alchemy".into(), ip.parse().unwrap());
            }
            c
        }));
        containers
    }

    #[test]
    fn relists_at_once_on_starts_and_network_changes_only() {
        let cases = [
            (ContainerAction::Start, true),
            (ContainerAction::Restart, true),
            (ContainerAction::NetworkConnect, true),
            (ContainerAction::NetworkDisconnect, true),
            (ContainerAction::Create, false),
            (ContainerAction::Stop, false),
            (ContainerAction::Die, false),
        ];
        for (action, due) in cases {
            let mut relists = Relists::default();
            relists.event(&ContainerEvent::new("a", action));
            assert_eq!(relists.start(), due.then_some(Duration::ZERO), "{action:?}");
            assert_eq!(relists.start(), None, "{action:?}: once");
        }
    }

    #[test]
    fn relists_again_until_a_network_change_shows() {
        let running = ContainerState::Running;
        let stale = containers(&[("a", running, None)]);
        let mut relists = Relists::default();
        relists.event(&ContainerEvent::new("a", ContainerAction::NetworkConnect));
        let mut waits = Vec::new();
        while let Some(delay) = relists.start() {
            waits.push(delay.as_millis());
            let before = relists.addresses(&stale);
            relists.listed(&before, &stale);
        }
        assert_eq!(waits, [0, 100, 200, 400, 800, 1600], "gives up after five");

        let mut relists = Relists::default();
        relists.event(&ContainerEvent::new("a", ContainerAction::NetworkConnect));
        assert_eq!(relists.start(), Some(Duration::ZERO));
        let before = relists.addresses(&stale);
        relists.listed(&before, &stale);
        assert_eq!(relists.start(), Some(Duration::from_millis(100)));
        let before = relists.addresses(&stale);
        relists.listed(&before, &containers(&[("a", running, Some("10.0.0.2"))]));
        assert_eq!(relists.start(), None, "the address showed");

        relists.event(&ContainerEvent::new("a", ContainerAction::Start));
        relists.event(&ContainerEvent::new("b", ContainerAction::NetworkConnect));
        assert_eq!(relists.start(), Some(Duration::ZERO));
        let created = containers(&[("a", running, None), ("b", ContainerState::Created, None)]);
        let before = relists.addresses(&created);
        relists.listed(&before, &created);
        assert_eq!(
            relists.start(),
            None,
            "a container that isn't running has no address to wait for"
        );
    }

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
            (Outcome::Woke, 0),
            (dropped, 1),
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
