//! The Alchemy daemon.
//!
//! `alchemyd` is the single source of truth. It serves the same HTTP API
//! ([`http`]) through two front doors over the same state:
//!
//! - a TCP listener, which adds the embedded web UI, CORS for the Tauri
//!   shell, and the [`guard`] checks against other web pages, used by
//!   browsers and the shell, and
//! - a per-user Unix socket ([`socket`]) serving the API alone, used by
//!   the `alchemy` CLI and `alchemy-helper`.
//!
//! It talks to the Docker engine through [`engine`], samples resource
//! usage through [`metrics`], keeps what it learns in [`State`], and
//! persists settings and history in a [`store::Store`].

pub mod compose;
pub mod engine;
pub mod events;
pub mod guard;
pub mod http;
pub mod metrics;
mod secure_fs;
pub mod socket;
mod state;
pub mod store;
mod web;

pub use state::State;

use std::future::{Future, IntoFuture as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use tokio::sync::watch;

use crate::engine::{EngineMonitor, EngineSource, SystemConnector, SystemProbe};
use crate::metrics::{Collector, MetricsSource, SystemMetrics};
use crate::store::{SharedStore, SqliteStore};

/// Default HTTP listen address. Loopback only: the API has no
/// authentication yet.
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

/// What [`run_with`] runs against: the shared state, the task that keeps
/// its engine part current, and where metrics are read.
#[derive(Debug, Clone)]
pub struct Deps<E, M = SystemMetrics> {
    /// State shared by the engine task, the metrics collector, and both
    /// front doors. Keep a clone to observe or seed it from outside.
    pub state: State,
    /// Keeps the engine status and containers in `state` current.
    pub engine: E,
    /// Where the metrics collector reads the machine and its containers.
    pub metrics: M,
}

impl<E> Deps<E> {
    /// Fresh state with `engine` keeping it current, and this machine's
    /// metrics.
    pub fn new(engine: E) -> Self {
        Self {
            state: State::new(),
            engine,
            metrics: SystemMetrics::new(),
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
        }
    }
}

impl Deps<EngineMonitor<SystemProbe, SystemConnector>> {
    /// The real engine monitor for this host.
    pub fn system() -> Self {
        Self::new(EngineMonitor::system())
    }
}

/// Bind both listeners and serve until `shutdown` resolves, monitoring
/// this host's Docker engine.
pub async fn run(config: Config, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
    run_with(config, Deps::system(), shutdown).await
}

/// [`run`] with injected dependencies, such as a fake engine in tests.
///
/// The state database in `config.state_dir` is opened, created, or
/// migrated first; a corrupt or too-new one fails startup before either
/// listener is bound. The engine task and the metrics collector run
/// alongside the listeners. An engine that is absent or broken never fails
/// startup; it only shows up in the engine status.
pub async fn run_with(
    config: Config,
    deps: Deps<impl EngineSource, impl MetricsSource>,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    if !config.listen.ip().is_loopback() {
        tracing::warn!(
            listen = %config.listen,
            "HTTP API is bound to a non-loopback address and has no authentication yet"
        );
    }

    let sqlite = SqliteStore::open(&config.state_dir)
        .await
        .context("opening the state database")?;
    let db = sqlite.path().to_owned();
    let store: SharedStore = Arc::new(sqlite);

    let (socket_listener, socket_file) = socket::bind(&config.socket)
        .with_context(|| format!("binding API socket {}", config.socket.display()))?;
    let http_listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding HTTP listener {}", config.listen))?;

    tracing::info!(
        version = alchemy_core::VERSION,
        http = %config.listen,
        socket = %config.socket.display(),
        state = %db.display(),
        "alchemyd listening"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let engine_task = tokio::spawn(deps.engine.run(deps.state.clone(), shutdown_rx.clone()));
    let metrics_task = tokio::spawn(
        Collector::new(deps.metrics, deps.state.clone(), store.clone()).run(shutdown_rx.clone()),
    );
    let socket_task = tokio::spawn(
        axum::serve(
            socket_listener,
            http::api_router(deps.state.clone(), store.clone(), shutdown_rx.clone()),
        )
        .with_graceful_shutdown(stopped(shutdown_rx.clone()))
        .into_future(),
    );
    let http_task = tokio::spawn(
        axum::serve(
            http_listener,
            http::router(deps.state, store, config.listen.ip(), shutdown_rx.clone()),
        )
        .with_graceful_shutdown(stopped(shutdown_rx))
        .into_future(),
    );

    shutdown.await;
    shutdown_tx.send_replace(true);

    let engine_result = engine_task.await;
    let metrics_result = metrics_task.await;
    let socket_result = socket_task.await;
    let http_result = http_task.await;
    drop(socket_file);
    engine_result.context("engine task failed")?;
    metrics_result.context("metrics task failed")?;
    socket_result.context("API socket task failed")??;
    http_result.context("HTTP task failed")??;
    Ok(())
}

async fn stopped(mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}
