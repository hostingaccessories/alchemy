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
//! on apps through [`operations`], keeps what it learns in [`State`], and
//! persists settings, history, and stop [`intent`] in a [`store::Store`].

pub mod apps;
pub mod auth;
pub mod compose;
pub mod engine;
pub mod events;
pub mod guard;
pub mod http;
pub mod intent;
pub mod logs;
pub mod metrics;
pub mod operations;
mod secure_fs;
pub mod session;
pub mod socket;
mod state;
pub mod store;
mod web;

pub use state::State;

use std::future::{Future, IntoFuture as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use tokio::sync::watch;

use crate::apps::{AppEditor, AppsDeps};
use crate::engine::{EngineMonitor, EngineSource, SystemConnector, SystemProbe};
use crate::logs::{SharedLogSource, SystemLogs};
use crate::metrics::{Collector, MetricsSource, SystemMetrics};
use crate::operations::{OperationRunner, Operations, SystemRunner};
use crate::session::{Auth, TokenFile};
use crate::store::{SharedStore, SqliteStore, StateLock};

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
}

impl<E> Deps<E> {
    /// Fresh state with `engine` keeping it current, and this machine's
    /// metrics.
    ///
    /// The apps task gets the real filesystem but no default home, so it
    /// searches nothing until the settings name a folder, and a
    /// [`FakeComposeConfig`](compose::FakeComposeConfig) that knows no
    /// projects, so `docker compose` never runs. Logs are read from the
    /// engine. Operations run on a
    /// [`FakeComposeRunner`](compose::FakeComposeRunner) that succeeds
    /// without running anything. The editor writes to the real
    /// filesystem but validates with a
    /// [`FakeComposeValidator`](compose::FakeComposeValidator) that
    /// accepts everything. Use [`Self::with_apps`],
    /// [`Self::with_runner`], and [`Self::with_editor`] to change them.
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
}

impl Deps<EngineMonitor<SystemProbe, SystemConnector>> {
    /// The real engine monitor, apps task, and `docker compose` for this
    /// host.
    pub fn system() -> Self {
        Self::new(EngineMonitor::system())
            .with_apps(AppsDeps::system())
            .with_runner(Arc::new(SystemRunner::new()))
            .with_editor(AppEditor::system())
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
/// listener is bound. The engine task, the metrics collector, and the
/// apps task run alongside the listeners. An engine that is absent or
/// broken never fails startup; it only shows up in the engine status.
/// Neither does a missing or unreadable app folder, which shows up in the
/// app list.
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
    match intent::load(&*store, &deps.state).await {
        Ok(count) => tracing::debug!(count, "stop intents loaded"),
        Err(error) => tracing::warn!(
            %error,
            "cannot read stop intents; apps Alchemy stopped may show as stopped outside it"
        ),
    }

    let operations = Operations::new(deps.runner, deps.state.clone(), store.clone());
    if let Err(error) = operations.recover().await {
        tracing::warn!(%error, "cannot tidy the operation history");
    }

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
    let engine_task = tokio::spawn(deps.engine.run(deps.state.clone(), shutdown_rx.clone()));
    let metrics_task = tokio::spawn(
        Collector::new(deps.metrics, deps.state.clone(), store.clone()).run(shutdown_rx.clone()),
    );
    let apps_task = tokio::spawn(apps::run(
        deps.apps,
        deps.state.clone(),
        store.clone(),
        shutdown_rx.clone(),
    ));
    let socket_task = tokio::spawn(
        axum::serve(
            socket_listener,
            http::with_editor(
                http::api_router(
                    deps.state.clone(),
                    store.clone(),
                    shutdown_rx.clone(),
                    deps.logs.clone(),
                    operations.clone(),
                    auth.clone(),
                ),
                deps.editor.clone(),
            ),
        )
        .with_graceful_shutdown(stopped(shutdown_rx.clone()))
        .into_future(),
    );
    let http_task = tokio::spawn(
        axum::serve(
            http_listener,
            http::with_editor(
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
                deps.editor,
            ),
        )
        .with_graceful_shutdown(stopped(shutdown_rx))
        .into_future(),
    );

    shutdown.await;
    shutdown_tx.send_replace(true);
    operations.shutdown().await;

    let engine_result = engine_task.await;
    let metrics_result = metrics_task.await;
    let apps_result = apps_task.await;
    let socket_result = socket_task.await;
    let http_result = http_task.await;
    drop(token_file);
    drop(socket_file);
    engine_result.context("engine task failed")?;
    metrics_result.context("metrics task failed")?;
    apps_result.context("apps task failed")?;
    socket_result.context("API socket task failed")??;
    http_result.context("HTTP task failed")??;
    Ok(())
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
