//! The Alchemy daemon.
//!
//! `alchemyd` is the single source of truth. It serves two front doors
//! over the same state:
//!
//! - an HTTP API plus the embedded web UI ([`http`]), used by browsers
//!   and the Tauri shell, and
//! - a local IPC socket ([`ipc`]), used by the `alchemy` CLI and
//!   `alchemy-helper`.

pub mod http;
pub mod ipc;
mod secure_fs;
mod web;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context as _;
use tokio::sync::watch;

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

/// Bind both listeners and serve until `shutdown` resolves.
pub async fn run(config: Config, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
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
    let ipc_task = tokio::spawn(ipc::serve(ipc_listener, shutdown_rx.clone()));
    let mut http_shutdown = shutdown_rx;
    let http_task = tokio::spawn(async move {
        axum::serve(http_listener, http::router())
            .with_graceful_shutdown(async move {
                let _ = http_shutdown.wait_for(|stop| *stop).await;
            })
            .await
    });

    shutdown.await;
    shutdown_tx.send_replace(true);

    let ipc_result = ipc_task.await;
    let http_result = http_task.await;
    ipc_result.context("IPC task failed")?;
    http_result.context("HTTP task failed")??;
    Ok(())
}
