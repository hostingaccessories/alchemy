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
//!
//! After alchemyd starts, the computer wakes, or Docker returns, the
//! [`reconcile`] task brings back the apps that were running, and
//! [`desktop`] starts Docker Desktop when someone asks.
//!
//! It notices the computer waking from sleep through [`wake`], and then
//! reads the LAN address again, announces `.local` names again, and
//! subscribes to Docker's events again.
//!
//! What needs root, such as port 80 on Linux, is granted through
//! `alchemy-helper` behind a system prompt by [`privileges`]. A change
//! that only takes effect at exec makes [`run`] return
//! [`Stopped::Restart`], and `main` re-executes the daemon with
//! [`restart::exec_self`].

pub mod apps;
pub mod auth;
pub mod autostart;
pub mod compose;
pub mod desktop;
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
pub mod privileges;
pub mod proxy;
pub mod reconcile;
pub mod restart;
pub mod routing;
mod secure_fs;
pub mod session;
pub mod socket;
mod state;
pub mod store;
pub mod subnets;
pub mod wake;
mod web;

pub use state::{EngineLink, State};

use std::future::{Future, IntoFuture as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use alchemy_core::lifecycle::Supervisor;
use alchemy_core::privilege::ProxyPorts;
use alchemy_core::route::RouteTable;
use anyhow::Context as _;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::apps::{AppEditor, AppsDeps};
use crate::autostart::{Autostart, AutostartDeps};
use crate::desktop::{FakeLauncher, SharedLauncher, SystemLauncher};
use crate::engine::{EngineMonitor, EngineSource, SystemConnector, SystemProbe};
use crate::logs::{SharedLogSource, SystemLogs};
use crate::metrics::{Collector, MetricsSource, SystemMetrics};
use crate::network::{FakeNetwork, SharedNetwork, SystemNetwork};
use crate::operations::{OperationRunner, Operations, SystemRunner};
use crate::ports::PortRegistry;
use crate::privileges::{PrivilegeDeps, Privileges};
use crate::proxy::ProxyStatus;
use crate::reconcile::ReconcileDeps;
use crate::routing::{ProxyDeps, ProxyHandle};
use crate::session::{Auth, TokenFile};
use crate::store::{SharedStore, SqliteStore, StateLock};
use crate::subnets::{EngineNetworks, FakeDaemonConfig, FakeEngineNetworks, SubnetsDeps};
use crate::wake::WakeDeps;

/// Default HTTP listen address. Loopback only: the TCP listener's token
/// keeps out other processes and users on this machine, but there is no
/// login fit for other machines yet.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7240";

/// How often an alchemyd under a service tries the state directory again
/// while another alchemyd holds it.
pub const LOCK_POLL: Duration = Duration::from_millis(250);

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
    /// Where the privileges' facts are read and how the helper is run as
    /// root.
    pub privileges: PrivilegeDeps,
    /// How Docker Desktop is started when someone asks.
    pub desktop: SharedLauncher,
    /// How the reconcile task is tuned.
    pub reconcile: ReconcileDeps,
    /// Where the clocks that tell a wake from sleep are read.
    pub wake: WakeDeps,
    /// What runs this alchemyd, and where the per-user service is
    /// installed.
    pub autostart: AutostartDeps,
}

/// Why [`run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The shutdown future resolved.
    Shutdown,
    /// A privilege change takes effect only when alchemyd starts again,
    /// so the caller should re-execute it with [`restart::exec_self`].
    Restart,
    /// Autostart switched from the per-user service running this alchemyd
    /// to the system service, whose alchemyd takes over. The caller should
    /// exit 0, so the service manager doesn't start this one again.
    HandedOver,
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
    /// listener, and a connector that reaches nothing. Privileges are
    /// [`PrivilegeDeps::fake`]. Docker Desktop is started through a
    /// [`FakeLauncher`] that runs nothing, the reconcile task has its
    /// default tuning, wakes are noticed on this machine's clocks, and
    /// autostart is [`AutostartDeps::fake`]. Use [`Self::with_apps`],
    /// [`Self::with_runner`], [`Self::with_editor`],
    /// [`Self::with_network`], [`Self::with_subnets`],
    /// [`Self::with_proxy`], [`Self::with_privileges`],
    /// [`Self::with_desktop`], [`Self::with_reconcile`],
    /// [`Self::with_wake`], and [`Self::with_autostart`] to change them.
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
            privileges: PrivilegeDeps::fake(),
            desktop: Arc::new(FakeLauncher::new()),
            reconcile: ReconcileDeps::default(),
            wake: WakeDeps::system(),
            autostart: AutostartDeps::fake(),
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
            privileges: self.privileges,
            desktop: self.desktop,
            reconcile: self.reconcile,
            wake: self.wake,
            autostart: self.autostart,
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

    /// Read and change privileges through `privileges`.
    #[must_use]
    pub fn with_privileges(mut self, privileges: PrivilegeDeps) -> Self {
        self.privileges = privileges;
        self
    }

    /// Start Docker Desktop through `desktop`.
    #[must_use]
    pub fn with_desktop(mut self, desktop: SharedLauncher) -> Self {
        self.desktop = desktop;
        self
    }

    /// Tune the reconcile task with `reconcile`.
    #[must_use]
    pub fn with_reconcile(mut self, reconcile: ReconcileDeps) -> Self {
        self.reconcile = reconcile;
        self
    }

    /// Tell a wake from sleep with `wake`.
    #[must_use]
    pub fn with_wake(mut self, wake: WakeDeps) -> Self {
        self.wake = wake;
        self
    }

    /// Read and change autostart through `autostart`.
    #[must_use]
    pub fn with_autostart(mut self, autostart: AutostartDeps) -> Self {
        self.autostart = autostart;
        self
    }
}

impl Deps<EngineMonitor<SystemProbe, SystemConnector>> {
    /// The real engine monitor, apps task, `docker compose`, host
    /// network, Docker networks, privileges, Docker Desktop launcher, and
    /// per-user service for this host.
    pub fn system() -> Self {
        let autostart = AutostartDeps::system();
        let supervisor = autostart.supervisor;
        Self::new(EngineMonitor::system())
            .with_apps(AppsDeps::system())
            .with_runner(Arc::new(SystemRunner::new()))
            .with_editor(AppEditor::system())
            .with_network(Arc::new(SystemNetwork::new()))
            .with_subnets(SubnetsDeps::system())
            .with_proxy(ProxyDeps::system())
            .with_privileges(PrivilegeDeps::system().with_supervisor(supervisor))
            .with_desktop(Arc::new(SystemLauncher))
            .with_autostart(autostart)
    }
}

/// Bind both listeners and serve until `shutdown` resolves or a privilege
/// change asks for a restart, monitoring this host's Docker engine.
pub async fn run(config: Config, shutdown: impl Future<Output = ()>) -> anyhow::Result<Stopped> {
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
/// task, the network check, the wake task, the web address proxy, and the
/// routing task that feeds it run alongside the listeners. An engine
/// that is absent or broken never fails startup; it only shows up in the
/// engine status. Neither does a missing or unreadable app folder, which
/// shows up in the app list, nor a network that overlaps, which shows up
/// in `GET /api/network`.
///
/// Once both listeners are bound, and before either serves a request, a
/// new TCP token is written to [`alchemy_api::TCP_TOKEN_FILE`] in the
/// state directory; it is removed again on shutdown.
///
/// The [`reconcile`] task starts alongside them, reading the apps last
/// seen running from the state database.
///
/// Operations a previous daemon left running are marked interrupted at
/// startup. On shutdown, running operations are interrupted and stored
/// before the listeners close.
///
/// A privilege change that takes effect only at exec shuts the daemon
/// down the same way, and it returns [`Stopped::Restart`].
pub async fn run_with(
    config: Config,
    deps: Deps<impl EngineSource, impl MetricsSource>,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<Stopped> {
    warn_if_exposed(config.listen);
    let mut shutdown = std::pin::pin!(shutdown);
    let supervisor = deps.autostart.supervisor;
    let opened = open_state(&config.state_dir, supervisor, shutdown.as_mut());
    let Some((_state_lock, sqlite)) = opened.await? else {
        return Ok(Stopped::Shutdown);
    };
    let db = sqlite.path().to_owned();
    let store: SharedStore = Arc::new(sqlite);
    load_stored(&store, &deps.apps, &deps.state).await;

    let ports = PortRegistry::new(deps.state.clone(), store.clone(), deps.network.clone());
    let operations = recovered_operations(deps.runner, &deps.state, &store, &ports).await;

    let (socket_listener, socket_file) = socket::bind(&config.socket)
        .with_context(|| format!("binding API socket {}", config.socket.display()))?;
    let http_listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding HTTP listener {}", config.listen))?;
    let (listen, auth, token_file) = sign_in_state(&http_listener, &config.state_dir)?;

    announce(listen, &config.socket, &db, token_file.path());

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (privileges, autostart, machine_tasks) = spawn_machine(
        (deps.privileges, deps.autostart),
        &deps.state,
        deps.proxy.range,
        &shutdown_rx,
    );
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
    background.extend(machine_tasks);
    background.extend([
        spawn_wake(deps.wake, &deps.state, &shutdown_rx),
        spawn_reconcile(
            deps.reconcile,
            &deps.state,
            &store,
            &operations,
            &shutdown_rx,
        ),
    ]);
    let machine = (&privileges, &autostart);
    let handlers = |router| with_handlers(router, &deps.editor, &ports, machine, &deps.desktop);
    let socket_task = serve(
        socket_listener,
        handlers(http::api_router(
            deps.state.clone(),
            store.clone(),
            shutdown_rx.clone(),
            deps.logs.clone(),
            operations.clone(),
            auth.clone(),
        )),
        &shutdown_rx,
    );
    let http_task = serve(
        http_listener,
        handlers(http::router(
            deps.state,
            store,
            http::Tcp {
                listen: listen.ip(),
                auth,
            },
            shutdown_rx.clone(),
            deps.logs,
            operations.clone(),
        )),
        &shutdown_rx,
    );

    let why = tokio::select! {
        () = shutdown.as_mut() => Stopped::Shutdown,
        () = privileges.restarting() => Stopped::Restart,
        () = autostart.handing_over() => Stopped::HandedOver,
    };
    shutdown_tx.send_replace(true);
    operations.shutdown().await;
    drain(
        background,
        [socket_task, http_task],
        (token_file, socket_file),
    )
    .await?;
    Ok(why)
}

/// Wait for every task to end, then remove the token and socket files in
/// `files`, and report the first task that failed.
async fn drain<S>(
    background: Vec<(&'static str, JoinHandle<()>)>,
    servers: [JoinHandle<std::io::Result<()>>; 2],
    files: (TokenFile, S),
) -> anyhow::Result<()> {
    let mut background_results = Vec::with_capacity(background.len());
    for (name, task) in background {
        background_results.push((name, task.await));
    }
    let [socket_task, http_task] = servers;
    let socket_result = socket_task.await;
    let http_result = http_task.await;
    drop(files);
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

/// Load the stop intents, app settings, and network settings in `store`
/// into `state`.
async fn load_stored(store: &SharedStore, apps: &AppsDeps, state: &State) {
    load_intents(store, state).await;
    apps::load_settings(apps, state, store).await;
    network::settings::load(state, store).await;
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

/// The daemon's [`Privileges`], read through `deps.0` with the firewall
/// opening the proxy's `range`, its [`Autostart`], through `deps.1`, and
/// the tasks that keep both current in `state`, with their names.
fn spawn_machine(
    deps: (PrivilegeDeps, AutostartDeps),
    state: &State,
    range: alchemy_core::ports::PortRange,
    shutdown: &watch::Receiver<bool>,
) -> (Privileges, Autostart, [(&'static str, JoinHandle<()>); 2]) {
    let privileges = Privileges::new(
        deps.0,
        state.clone(),
        ProxyPorts::try_from(range).unwrap_or_default(),
    );
    let autostart = Autostart::new(deps.1, privileges.clone(), state.clone());
    let tasks = [
        (
            "privileges",
            tokio::spawn(privileges.clone().run(shutdown.clone())),
        ),
        (
            "autostart",
            tokio::spawn(autostart.clone().run(shutdown.clone())),
        ),
    ];
    (privileges, autostart, tasks)
}

/// Start the [`reconcile`] task, tuned by `deps`, starting apps through
/// `operations`, with its name.
fn spawn_reconcile(
    deps: ReconcileDeps,
    state: &State,
    store: &SharedStore,
    operations: &Operations,
    shutdown: &watch::Receiver<bool>,
) -> (&'static str, JoinHandle<()>) {
    let task = reconcile::run(
        deps,
        state.clone(),
        store.clone(),
        operations.clone(),
        shutdown.clone(),
    );
    ("reconcile", tokio::spawn(task))
}

/// Start the [`wake`] task, with its name.
fn spawn_wake(
    deps: WakeDeps,
    state: &State,
    shutdown: &watch::Receiver<bool>,
) -> (&'static str, JoinHandle<()>) {
    (
        "wake",
        tokio::spawn(wake::run(deps, state.clone(), shutdown.clone())),
    )
}

/// Serve `router` on `listener` until `shutdown` becomes `true`, letting
/// requests in flight finish.
fn serve<L>(
    listener: L,
    router: axum::Router,
    shutdown: &watch::Receiver<bool>,
) -> JoinHandle<std::io::Result<()>>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
{
    tokio::spawn(
        axum::serve(listener, router)
            .with_graceful_shutdown(stopped(shutdown.clone()))
            .into_future(),
    )
}

/// Give `router` the app editor, the port registry, the privileges and
/// autostart in `machine`, and the Docker Desktop launcher its routes
/// use.
fn with_handlers(
    router: axum::Router,
    editor: &Arc<AppEditor>,
    ports: &PortRegistry,
    machine: (&Privileges, &Autostart),
    desktop: &SharedLauncher,
) -> axum::Router {
    let router = http::with_ports(http::with_editor(router, editor.clone()), ports.clone());
    let router = http::with_privileges(router, machine.0.clone());
    let router = http::with_autostart(router, machine.1.clone());
    http::with_desktop(router, desktop.clone())
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

/// Log where this alchemyd listens and keeps its state.
fn announce(listen: SocketAddr, socket: &Path, db: &Path, token: &Path) {
    tracing::info!(
        version = alchemy_core::VERSION,
        http = %listen,
        socket = %socket.display(),
        state = %db.display(),
        token = %token.display(),
        "alchemyd listening"
    );
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
///
/// Under a service, as `supervisor` says, a directory another alchemyd
/// holds is tried again every [`LOCK_POLL`] until it is free, as during a
/// handover between services, or until `shutdown` resolves, which gives
/// `None`.
async fn open_state(
    state_dir: &Path,
    supervisor: Supervisor,
    mut shutdown: Pin<&mut impl Future<Output = ()>>,
) -> anyhow::Result<Option<(StateLock, SqliteStore)>> {
    let mut told = false;
    let lock = loop {
        match store::lock_state_dir(state_dir) {
            Ok(lock) => break lock,
            Err(store::StoreError::InUse { .. }) if supervisor.waits_for_state() => {
                if !told {
                    tracing::info!(
                        state = %state_dir.display(),
                        "another alchemyd is using the state directory; waiting for it to exit"
                    );
                    told = true;
                }
                tokio::select! {
                    () = shutdown.as_mut() => return Ok(None),
                    () = tokio::time::sleep(LOCK_POLL) => {}
                }
            }
            Err(error) => return Err(error).context("locking the state directory"),
        }
    };
    let sqlite = SqliteStore::open(state_dir)
        .await
        .context("opening the state database")?;
    Ok(Some((lock, sqlite)))
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
