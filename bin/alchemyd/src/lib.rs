//! The Alchemy daemon.
//!
//! `alchemyd` is the single source of truth. It serves the same HTTP API
//! ([`http`]) through two front doors over the same state:
//!
//! - a TCP listener, which adds the embedded web UI, CORS for the Tauri
//!   shell, the [`guard`] checks against other web pages, and signing in
//!   with this start's token or a browser session ([`auth`], [`session`]),
//!   used by browsers and the shell, and
//! - a per-user Unix socket ([`socket`]) serving the API alone, used by
//!   the `alchemy` CLI and `alchemy-helper`.
//!
//! It talks to the Docker engine through [`engine`], finds the user's
//! Compose stacks through [`apps`], samples resource usage through
//! [`metrics`], reads app logs through [`logs`], runs lifecycle actions
//! on apps through [`operations`], reads the host's own subnets, routes,
//! and listeners through [`network`], lists and reserves host ports
//! through [`ports`], checks Docker's networks against the host's and
//! keeps its own `alchemy` network through [`subnets`], keeps what it
//! learns in [`State`], and persists settings, history, and stop
//! [`intent`] in a [`store::Store`].
//!
//! Apps get web addresses from the [`proxy`], on listeners of its own
//! that never serve the API, with the routes [`routing`] plans for it,
//! and their `.local` names are announced to the LAN through [`mdns`].

pub mod apps;
pub mod auth;
pub mod compose;
pub mod engine;
pub mod events;
pub mod guard;
pub mod http;
pub mod intent;
pub mod logs;
pub mod mdns;
pub mod metrics;
pub mod network;
pub mod operations;
pub mod ports;
pub mod proxy;
pub mod routing;
mod secure_fs;
pub mod session;
pub mod socket;
mod state;
pub mod store;
pub mod subnets;
mod web;

pub use state::State;

use std::future::{Future, IntoFuture as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alchemy_core::route::RouteTable;
use anyhow::Context as _;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::apps::{AppEditor, AppsDeps};
use crate::engine::{EngineMonitor, EngineSource, SystemConnector, SystemProbe};
use crate::logs::{SharedLogSource, SystemLogs};
use crate::metrics::{Collector, MetricsSource, SystemMetrics};
use crate::network::{FakeNetwork, SharedNetwork, SystemNetwork};
use crate::operations::{OperationRunner, Operations, SystemRunner};
use crate::ports::PortRegistry;
use crate::proxy::ProxyStatus;
use crate::routing::{ProxyDeps, ProxyHandle};
use crate::session::{Auth, TokenFile};
use crate::store::{SharedStore, SqliteStore, StateLock};
use crate::subnets::{EngineNetworks, FakeDaemonConfig, FakeEngineNetworks, SubnetsDeps};

/// Default HTTP listen address. Loopback only: the TCP listener's token
/// keeps out other processes and users on this machine, but there is no
/// login fit for other machines yet.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7240";

/// Runtime configuration for [`run`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Address for the HTTP API and web UI.
    pub listen: SocketAddr,
    /// Path of the per-user Unix socket the API is served on.
    pub socket: PathBuf,
    /// Directory holding the state database, usually
    /// [`store::default_state_dir`].
    pub state_dir: PathBuf,
}

/// What [`run_with`] runs against: the shared state, the tasks that keep
/// it current, and where metrics are read.
#[derive(Debug, Clone)]
pub struct Deps<E, M = SystemMetrics> {
    /// State shared by the background tasks and both front doors. Keep a
    /// clone to observe or seed it from outside.
    pub state: State,
    /// Keeps the engine status and containers in `state` current.
    pub engine: E,
    /// Where the metrics collector reads the machine and its containers.
    pub metrics: M,
    /// What the apps task lists folders and reads compose files through.
    pub apps: AppsDeps,
    /// Where app logs are read.
    pub logs: SharedLogSource,
    /// What lifecycle operations run their Compose plans through.
    pub runner: Arc<dyn OperationRunner>,
    /// Creates apps and edits their files for the API.
    pub editor: Arc<AppEditor>,
    /// Where the host's LAN subnets, routes, and listeners are read.
    pub network: SharedNetwork,
    /// Where Docker's networks and `daemon.json` are read, and the
    /// `alchemy` network is created and joined.
    pub subnets: SubnetsDeps,
    /// Where the web address proxy listens and how it reaches apps.
    pub proxy: ProxyDeps,
}

impl<E> Deps<E> {
    /// Fresh state with `engine` keeping it current, and this machine's
    /// metrics.
    ///
    /// The apps task gets the real filesystem but no default home, so it
    /// searches nothing until the settings name a folder, leaves staging
    /// folders alone, and gets a
    /// [`FakeComposeConfig`](compose::FakeComposeConfig) that knows no
    /// projects, so `docker compose` never runs. Logs are read from the
    /// engine. Operations run on a
    /// [`FakeComposeRunner`](compose::FakeComposeRunner) that succeeds
    /// without running anything. The editor writes to the real
    /// filesystem but validates with a
    /// [`FakeComposeValidator`](compose::FakeComposeValidator) that
    /// accepts everything. The host network is a
    /// [`FakeNetwork`] with nothing on it, and Docker's networks are a
    /// [`FakeEngineNetworks`] with none and no `daemon.json`. The proxy
    /// is [`ProxyDeps::loopback`]: app ports on loopback, no Host
    /// listener, and a connector that reaches nothing. Use
    /// [`Self::with_apps`], [`Self::with_runner`], [`Self::with_editor`],
    /// [`Self::with_network`], [`Self::with_subnets`], and
    /// [`Self::with_proxy`] to change them.
    pub fn new(engine: E) -> Self {
        Self {
            state: State::new(),
            engine,
            metrics: SystemMetrics::new(),
            apps: AppsDeps::new(
                Arc::new(apps::SystemFs),
                Arc::new(compose::FakeComposeConfig::new()),
                None,
            ),
            logs: Arc::new(SystemLogs::new()),
            runner: Arc::new(compose::FakeComposeRunner::new()),
            editor: Arc::new(AppEditor::new(
                Arc::new(apps::SystemDisk),
                Arc::new(compose::FakeComposeValidator::new()),
            )),
            network: Arc::new(FakeNetwork::new()),
            subnets: SubnetsDeps::new(
                Arc::new(FakeEngineNetworks::new()),
                Arc::new(FakeDaemonConfig::new()),
            ),
            proxy: ProxyDeps::loopback(),
        }
    }
}

impl<E, M> Deps<E, M> {
    /// The same, reading metrics from `metrics` instead.
    pub fn with_metrics<N>(self, metrics: N) -> Deps<E, N> {
        Deps {
            state: self.state,
            engine: self.engine,
            metrics,
            apps: self.apps,
            logs: self.logs,
            runner: self.runner,
            editor: self.editor,
            network: self.network,
            subnets: self.subnets,
            proxy: self.proxy,
        }
    }

    /// Use `apps` for the apps task.
    #[must_use]
    pub fn with_apps(mut self, apps: AppsDeps) -> Self {
        self.apps = apps;
        self
    }

    /// Read app logs from `logs`.
    #[must_use]
    pub fn with_logs(mut self, logs: SharedLogSource) -> Self {
        self.logs = logs;
        self
    }

    /// Run lifecycle operations through `runner`.
    #[must_use]
    pub fn with_runner(mut self, runner: Arc<dyn OperationRunner>) -> Self {
        self.runner = runner;
        self
    }

    /// Use `editor` to create apps and edit their files.
    #[must_use]
    pub fn with_editor(mut self, editor: AppEditor) -> Self {
        self.editor = Arc::new(editor);
        self
    }

    /// Read the host's subnets, routes, and listeners from `network`.
    #[must_use]
    pub fn with_network(mut self, network: SharedNetwork) -> Self {
        self.network = network;
        self
    }

    /// Read Docker's networks and `daemon.json` through `subnets`.
    #[must_use]
    pub fn with_subnets(mut self, subnets: SubnetsDeps) -> Self {
        self.subnets = subnets;
        self
    }

    /// Run the web address proxy with `proxy`.
    #[must_use]
    pub fn with_proxy(mut self, proxy: ProxyDeps) -> Self {
        self.proxy = proxy;
        self
    }
}

impl Deps<EngineMonitor<SystemProbe, SystemConnector>> {
    /// The real engine monitor, apps task, `docker compose`, host
    /// network, and Docker networks for this host.
    pub fn system() -> Self {
        Self::new(EngineMonitor::system())
            .with_apps(AppsDeps::system())
            .with_runner(Arc::new(SystemRunner::new()))
            .with_editor(AppEditor::system())
            .with_network(Arc::new(SystemNetwork::new()))
            .with_subnets(SubnetsDeps::system())
            .with_proxy(ProxyDeps::system())
    }
}

/// Bind both listeners and serve until `shutdown` resolves, monitoring
/// this host's Docker engine.
pub async fn run(config: Config, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
    run_with(config, Deps::system(), shutdown).await
}

/// [`run`] with injected dependencies, such as a fake engine in tests.
///
/// `config.state_dir` is locked first, so a second daemon on the same
/// directory fails startup before it touches anything; the lock is held
/// until this returns. The state database in it is then opened, created,
/// or migrated; a corrupt or too-new one fails startup before either
/// listener is bound, and the app and network settings in it are loaded
/// into the state before anything is served. The engine task, the metrics collector, the apps
/// task, the network check, the web address proxy, and the routing task
/// that feeds it run alongside the listeners. An engine
/// that is absent or broken never fails startup; it only shows up in the
/// engine status. Neither does a missing or unreadable app folder, which
/// shows up in the app list, nor a network that overlaps, which shows up
/// in `GET /api/network`.
///
/// Once both listeners are bound, and before either serves a request, a
/// new TCP token is written to [`alchemy_api::TCP_TOKEN_FILE`] in the
/// state directory; it is removed again on shutdown.
///
/// Operations a previous daemon left running are marked interrupted at
/// startup. On shutdown, running operations are interrupted and stored
/// before the listeners close.
pub async fn run_with(
    config: Config,
    deps: Deps<impl EngineSource, impl MetricsSource>,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    warn_if_exposed(config.listen);
    let (_state_lock, sqlite) = open_state(&config.state_dir).await?;
    let db = sqlite.path().to_owned();
    let store: SharedStore = Arc::new(sqlite);
    load_intents(&store, &deps.state).await;
    apps::load_settings(&deps.apps, &deps.state, &store).await;
    network::settings::load(&deps.state, &store).await;

    let ports = PortRegistry::new(deps.state.clone(), store.clone(), deps.network.clone());
    let operations = recovered_operations(deps.runner, &deps.state, &store, &ports).await;

    let (socket_listener, socket_file) = socket::bind(&config.socket)
        .with_context(|| format!("binding API socket {}", config.socket.display()))?;
    let http_listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding HTTP listener {}", config.listen))?;
    let (listen, auth, token_file) = sign_in_state(&http_listener, &config.state_dir)?;

    tracing::info!(
        version = alchemy_core::VERSION,
        http = %listen,
        socket = %config.socket.display(),
        state = %db.display(),
        token = %token_file.path().display(),
        "alchemyd listening"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut background = Vec::from(spawn_proxy(
        deps.proxy,
        listen,
        (deps.subnets.engine.clone(), deps.network.clone(), &ports),
        &deps.state,
        &shutdown_rx,
    ));
    background.extend(spawn_background(
        deps.engine,
        deps.metrics,
        deps.apps,
        (deps.subnets, deps.network.clone()),
        &deps.state,
        &store,
        &shutdown_rx,
    ));
    let socket_task = tokio::spawn(
        axum::serve(
            socket_listener,
            with_handlers(
                http::api_router(
                    deps.state.clone(),
                    store.clone(),
                    shutdown_rx.clone(),
                    deps.logs.clone(),
                    operations.clone(),
                    auth.clone(),
                ),
                &deps.editor,
                &ports,
            ),
        )
        .with_graceful_shutdown(stopped(shutdown_rx.clone()))
        .into_future(),
    );
    let http_task = tokio::spawn(
        axum::serve(
            http_listener,
            with_handlers(
                http::router(
                    deps.state,
                    store,
                    http::Tcp {
                        listen: listen.ip(),
                        auth,
                    },
                    shutdown_rx.clone(),
                    deps.logs,
                    operations.clone(),
                ),
                &deps.editor,
                &ports,
            ),
        )
        .with_graceful_shutdown(stopped(shutdown_rx.clone()))
        .into_future(),
    );

    shutdown.await;
    shutdown_tx.send_replace(true);
    operations.shutdown().await;

    let mut background_results = Vec::with_capacity(background.len());
    for (name, task) in background {
        background_results.push((name, task.await));
    }
    let socket_result = socket_task.await;
    let http_result = http_task.await;
    drop(token_file);
    drop(socket_file);
    for (name, result) in background_results {
        result.with_context(|| format!("{name} task failed"))?;
    }
    socket_result.context("API socket task failed")??;
    http_result.context("HTTP task failed")??;
    Ok(())
}

/// Load the stop intents in `store` into `state`. A failure only means
/// apps Alchemy stopped may show as stopped outside it.
async fn load_intents(store: &SharedStore, state: &State) {
    match intent::load(&**store, state).await {
        Ok(count) => tracing::debug!(count, "stop intents loaded"),
        Err(error) => tracing::warn!(
            %error,
            "cannot read stop intents; apps Alchemy stopped may show as stopped outside it"
        ),
    }
}

/// The daemon's operations, with what a daemon that stopped mid-run left
/// running marked interrupted.
async fn recovered_operations(
    runner: Arc<dyn OperationRunner>,
    state: &State,
    store: &SharedStore,
    ports: &PortRegistry,
) -> Operations {
    let operations = Operations::new(runner, state.clone(), store.clone(), ports.clone());
    if let Err(error) = operations.recover().await {
        tracing::warn!(%error, "cannot tidy the operation history");
    }
    operations
}

/// Give `router` the app editor and the port registry its routes use.
fn with_handlers(
    router: axum::Router,
    editor: &Arc<AppEditor>,
    ports: &PortRegistry,
) -> axum::Router {
    http::with_ports(http::with_editor(router, editor.clone()), ports.clone())
}

/// Start the tasks that keep `state` current, each with its name: the
/// engine monitor, the metrics collector, the apps task, and the network
/// check, which reads the host through `subnets.1`.
fn spawn_background(
    engine: impl EngineSource,
    metrics: impl MetricsSource,
    apps: AppsDeps,
    subnets: (SubnetsDeps, SharedNetwork),
    state: &State,
    store: &SharedStore,
    shutdown: &watch::Receiver<bool>,
) -> [(&'static str, JoinHandle<()>); 4] {
    let (subnets, network) = subnets;
    [
        (
            "engine",
            tokio::spawn(engine.run(state.clone(), shutdown.clone())),
        ),
        (
            "metrics",
            tokio::spawn(
                Collector::new(metrics, state.clone(), store.clone()).run(shutdown.clone()),
            ),
        ),
        (
            "apps",
            tokio::spawn(apps::run(
                apps,
                state.clone(),
                store.clone(),
                shutdown.clone(),
            )),
        ),
        (
            "network check",
            tokio::spawn(subnets::run(
                subnets,
                network,
                state.clone(),
                store.clone(),
                shutdown.clone(),
            )),
        ),
    ]
}

/// Start the web address [`proxy`] as `deps` says, never routing to
/// `api`, the [`routing`] task that feeds it routes, and the [`mdns`]
/// task that announces the `.local` names routing picks, each with its
/// name. The proxy starts with an empty route table, so it binds nothing
/// until routes arrive. Routing attaches containers through `routing.0`,
/// reads the LAN address through `routing.1`, and reserves ports through
/// `routing.2`.
fn spawn_proxy(
    deps: ProxyDeps,
    api: SocketAddr,
    routing: (Arc<dyn EngineNetworks>, SharedNetwork, &PortRegistry),
    state: &State,
    shutdown: &watch::Receiver<bool>,
) -> [(&'static str, JoinHandle<()>); 3] {
    let (engine, network, ports) = routing;
    let (routes_tx, routes_rx) = watch::channel(RouteTable::default());
    let (names_tx, names_rx) = watch::channel(None);
    let mdns = tokio::spawn(mdns::run(
        deps.responder.clone(),
        names_rx,
        state.clone(),
        deps.interval,
        shutdown.clone(),
    ));
    let (status_tx, status_rx) = watch::channel(ProxyStatus::default());
    let (probe_tx, probe_rx) = watch::channel(None);
    let proxy = tokio::spawn(proxy::run(
        deps.config(vec![api]),
        deps.connector.clone(),
        routes_rx,
        status_tx,
        probe_rx,
        shutdown.clone(),
    ));
    let handle = ProxyHandle {
        routes: routes_tx,
        names: names_tx,
        status: status_rx,
        probe: probe_tx,
    };
    let routing = tokio::spawn(routing::run(
        deps,
        engine,
        network,
        state.clone(),
        ports.clone(),
        handle,
        shutdown.clone(),
    ));
    [("routing", routing), ("proxy", proxy), ("mdns", mdns)]
}

fn warn_if_exposed(listen: SocketAddr) {
    if !listen.ip().is_loopback() {
        tracing::warn!(
            %listen,
            "HTTP API is bound to a non-loopback address, where its token travels in the clear"
        );
    }
}

/// Lock `state_dir`, then open its database. The database is never opened
/// while another daemon holds the directory.
async fn open_state(state_dir: &Path) -> anyhow::Result<(StateLock, SqliteStore)> {
    let lock = store::lock_state_dir(state_dir).context("locking the state directory")?;
    let sqlite = SqliteStore::open(state_dir)
        .await
        .context("opening the state database")?;
    Ok((lock, sqlite))
}

/// The address `listener` is bound to, and a new token for it, written to
/// its file in `state_dir`.
fn sign_in_state(
    listener: &tokio::net::TcpListener,
    state_dir: &Path,
) -> anyhow::Result<(SocketAddr, Auth, TokenFile)> {
    let listen = listener
        .local_addr()
        .context("reading the HTTP listener's address")?;
    let auth = Auth::generate(listen).context("making the TCP token")?;
    let path = alchemy_api::tcp_token_path(state_dir);
    let file = TokenFile::write(&path, auth.token())
        .with_context(|| format!("writing the TCP token to {}", path.display()))?;
    Ok((listen, auth, file))
}

async fn stopped(mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}
