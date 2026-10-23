//! The `alchemy` CLI.
//!
//! Each subcommand maps to one IPC request against `alchemyd`. The CLI
//! holds no state of its own.

mod error;
mod transport;

use std::path::PathBuf;

use alchemy_ipc::{Request, Response};
use clap::{Parser, Subcommand};

pub use error::ClientError;
pub use transport::exchange_at;

/// Manage Docker Compose stacks through alchemyd.
#[derive(Debug, Parser)]
#[command(name = "alchemy", version, about)]
pub struct Cli {
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    /// Path of the alchemyd IPC socket.
    #[arg(long, global = true, env = alchemy_ipc::SOCKET_ENV)]
    pub socket: Option<PathBuf>,

    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show whether alchemyd is reachable and which version it runs.
    Status,
}

/// Run `cli` and return the text to print on success.
pub async fn run(cli: Cli) -> Result<String, ClientError> {
    let socket = cli.socket.unwrap_or_else(alchemy_ipc::default_socket_path);
    match cli.command {
        Command::Status => status(&socket, cli.json).await,
    }
}

async fn status(socket: &std::path::Path, json: bool) -> Result<String, ClientError> {
    match exchange_at(socket, &Request::Version).await? {
        Response::Version { daemon, protocol } => Ok(if json {
            serde_json::json!({ "daemon": daemon, "protocol": protocol, "socket": socket })
                .to_string()
        } else {
            format!(
                "alchemyd {daemon} (protocol {protocol}) at {}",
                socket.display()
            )
        }),
        other => Err(ClientError::from_response(other)),
    }
}
