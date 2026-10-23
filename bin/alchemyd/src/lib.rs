//! The Alchemy daemon.
//!
//! `alchemyd` is the single source of truth. It serves two front doors
//! over the same state:
//!
//! - an HTTP API plus the embedded web UI ([`http`]), used by browsers
//!   and the Tauri shell, and
//! - a local IPC socket ([`ipc`]), used by the `alchemy` CLI and
//!   `alchemy-helper`.
//!
//! It talks to the Docker engine through [`engine`], and keeps what it
//! learns in [`State`].

pub mod compose;
pub mod engine;
pub mod events;
pub mod http;
pub mod ipc;
mod secure_fs;
mod state;
mod web;

pub use state::State;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context as _;
use tokio::sync::watch;

use crate::engine::{EngineMonitor, EngineSource, SystemConnector, SystemProbe};

/// Default HTTP listen address. Loopback only: the API has no
/// authentication yet.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7240";

/// Runtime configuration for [`run`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Address for the HTTP API and web UI.
    pub listen: SocketAddr,
    /// Path of the local IPC socket.
    pub socket: PathBuf,
}

/// What [`run_with`] runs against: the shared state and the task that
/// keeps its engine part current.
#[derive(Debug, Clone)]
pub struct Deps<E> {
    /// State shared by the engine task and both front doors. Keep a clone
    /// to observe or seed it from outside.
    pub state: State,
    /// Keeps the engine status and containers in `state` current.
    pub engine: E,
}

impl<E> Deps<E> {
    /// Fresh state with `engine` keeping it current.
    pub fn new(engine: E) -> Self {
        Self {
            state: State::new(),
            engine,
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
/// The engine task runs alongside the listeners. An engine that is absent
/// or broken never fails startup; it only shows up in the engine status.
pub async fn run_with(
    config: Config,
    deps: Deps<impl EngineSource>,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    if !config.listen.ip().is_loopback() {
        tracing::warn!(
            listen = %config.listen,
            "HTTP API is bound to a non-loopback address and has no authentication yet"
        );
    }

    let ipc_listener = ipc::bind(&config.socket)
        .with_context(|| format!("binding IPC socket {}", config.socket.display()))?;
    let http_listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding HTTP listener {}", config.listen))?;

    tracing::info!(
        version = alchemy_core::VERSION,
        http = %config.listen,
        socket = %config.socket.display(),
        "alchemyd listening"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let engine_task = tokio::spawn(deps.engine.run(deps.state.clone(), shutdown_rx.clone()));
    let ipc_task = tokio::spawn(ipc::serve(
        ipc_listener,
        deps.state.clone(),
        shutdown_rx.clone(),
    ));
    let router = http::router(deps.state, shutdown_rx.clone());
    let mut http_shutdown = shutdown_rx;
    let http_task = tokio::spawn(async move {
        axum::serve(http_listener, router)
            .with_graceful_shutdown(async move {
                let _ = http_shutdown.wait_for(|stop| *stop).await;
            })
            .await
    });

    shutdown.await;
    shutdown_tx.send_replace(true);

    let engine_result = engine_task.await;
    let ipc_result = ipc_task.await;
    let http_result = http_task.await;
    engine_result.context("engine task failed")?;
    ipc_result.context("IPC task failed")?;
    http_result.context("HTTP task failed")??;
    Ok(())
}
