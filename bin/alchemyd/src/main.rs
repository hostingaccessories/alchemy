//! `alchemyd` entry point: parse arguments, install logging, run the daemon
//! until SIGINT or SIGTERM, and re-execute it when a privilege change asks.

use std::io::IsTerminal as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Alchemy daemon.
#[derive(Debug, Parser)]
#[command(name = "alchemyd", version, about)]
struct Cli {
    /// Address for the HTTP API and web UI.
    #[arg(long, env = "ALCHEMY_LISTEN", default_value = alchemyd::DEFAULT_LISTEN)]
    listen: SocketAddr,

    /// Path of the API socket for the CLI. Defaults to the per-user runtime dir.
    #[arg(long, env = alchemy_api::SOCKET_ENV)]
    socket: Option<PathBuf>,

    /// Directory for the state database. Defaults to the per-user state dir.
    #[arg(long, env = alchemyd::store::STATE_DIR_ENV)]
    state_dir: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(
            EnvFilter::try_from_env("ALCHEMY_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let Some(state_dir) = cli.state_dir.or_else(alchemyd::store::default_state_dir) else {
        tracing::error!(
            "cannot find a state directory: set HOME, or pass --state-dir or {}",
            alchemyd::store::STATE_DIR_ENV
        );
        return ExitCode::FAILURE;
    };

    let config = alchemyd::Config {
        listen: cli.listen,
        socket: cli.socket.unwrap_or_else(alchemy_api::default_socket_path),
        state_dir,
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "failed to start async runtime");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(alchemyd::run(config, shutdown_signal())) {
        Ok(alchemyd::Stopped::Shutdown) => ExitCode::SUCCESS,
        Ok(alchemyd::Stopped::Restart) => {
            runtime.shutdown_timeout(Duration::from_secs(5));
            let error = alchemyd::restart::exec_self();
            tracing::error!(%error, "alchemyd couldn't restart itself");
            ExitCode::FAILURE
        }
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "alchemyd exited with an error");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
    tracing::info!("shutdown requested");
}
