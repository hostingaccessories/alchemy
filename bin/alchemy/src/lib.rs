//! The `alchemy` CLI.
//!
//! Each subcommand maps to IPC requests against `alchemyd`. The CLI
//! holds no state of its own.

mod error;
mod render;
mod transport;

use std::io::Write;
use std::path::{Path, PathBuf};

use alchemy_core::engine::EngineStatus;
use alchemy_ipc::{ErrorCode, Request, Response};
use clap::{Parser, Subcommand};

use crate::render::Daemon;
use crate::transport::{Connection, with_timeout};

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
    /// Show whether alchemyd is reachable, which version it runs, and
    /// whether Docker is ready.
    Status {
        /// Keep running and print the Docker status again each time it
        /// changes, until interrupted.
        #[arg(long)]
        watch: bool,
    },
}

/// Run `cli`, writing its output to `out`.
///
/// `status --watch` only returns on an error, such as the daemon
/// stopping; the caller ends it by dropping the future.
pub async fn run<W: Write>(cli: Cli, out: &mut W) -> Result<(), ClientError> {
    let socket = cli.socket.unwrap_or_else(alchemy_ipc::default_socket_path);
    match cli.command {
        Command::Status { watch: false } => status(&socket, cli.json, out).await,
        Command::Status { watch: true } => watch_status(&socket, cli.json, out).await,
    }
}

async fn status<W: Write>(socket: &Path, json: bool, out: &mut W) -> Result<(), ClientError> {
    let (daemon, engine) = with_timeout(socket, async {
        let mut conn = Connection::connect(socket).await?;
        let daemon = version(&mut conn).await?;
        let engine = engine_status(conn.request(&Request::Engine).await?, &daemon)?;
        Ok((daemon, engine))
    })
    .await?;
    print_status(out, &daemon, socket, &engine, json)
}

/// Follow the engine status. Only the handshake, up to the first status,
/// is bounded by a timeout: the stream may be quiet for as long as Docker
/// is.
async fn watch_status<W: Write>(socket: &Path, json: bool, out: &mut W) -> Result<(), ClientError> {
    let (mut conn, daemon, mut engine) = with_timeout(socket, async {
        let mut conn = Connection::connect(socket).await?;
        let daemon = version(&mut conn).await?;
        let first = conn.request(&Request::WatchEngine).await?;
        let engine = engine_status(first, &daemon)?;
        Ok((conn, daemon, engine))
    })
    .await?;
    print_status(out, &daemon, socket, &engine, json)?;
    let mut last_line = render::engine_line(&engine);
    loop {
        let Some(resp) = conn.recv().await? else {
            return Err(ClientError::Disconnected);
        };
        engine = engine_status(resp, &daemon)?;
        if json {
            writeln!(out, "{}", render::status_json(&daemon, socket, &engine))?;
        } else {
            let line = render::engine_line(&engine);
            if line == last_line {
                continue;
            }
            writeln!(out, "{line}")?;
            last_line = line;
        }
        out.flush()?;
    }
}

async fn version(conn: &mut Connection) -> Result<Daemon, ClientError> {
    match conn.request(&Request::Version).await? {
        Response::Version { daemon, protocol } => Ok(Daemon {
            version: daemon,
            protocol,
        }),
        other => Err(ClientError::from_response(other)),
    }
}

/// Unwrap an engine status, treating `BadRequest` as a daemon that
/// predates the engine requests.
fn engine_status(resp: Response, daemon: &Daemon) -> Result<EngineStatus, ClientError> {
    match resp {
        Response::Engine(engine) => Ok(engine),
        Response::Error {
            code: ErrorCode::BadRequest,
            ..
        } => Err(ClientError::DaemonTooOld {
            daemon: daemon.version.clone(),
        }),
        other => Err(ClientError::from_response(other)),
    }
}

fn print_status<W: Write>(
    out: &mut W,
    daemon: &Daemon,
    socket: &Path,
    engine: &EngineStatus,
    json: bool,
) -> Result<(), ClientError> {
    if json {
        writeln!(out, "{}", render::status_json(daemon, socket, engine))?;
    } else {
        writeln!(out, "{}", render::daemon_line(daemon, socket))?;
        writeln!(out, "{}", render::engine_line(engine))?;
    }
    out.flush()?;
    Ok(())
}
