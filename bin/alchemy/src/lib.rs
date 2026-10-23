//! The `alchemy` CLI.
//!
//! Each subcommand maps to IPC requests against `alchemyd`. The CLI
//! holds no state of its own.

pub mod commands;
mod context;
mod daemon;
mod error;
mod output;
mod target;
mod transport;

use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::commands::status;
use crate::context::Context;
use crate::daemon::IpcDaemon;
use crate::output::Output;
use crate::target::{Sources, Target};

pub use error::ClientError;
pub use target::DAEMON_ENV;
pub use transport::exchange_at;

/// Manage Docker Compose stacks through alchemyd.
#[derive(Debug, Parser)]
#[command(name = "alchemy", version, about)]
pub struct Cli {
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    /// URL of alchemyd, overriding [`DAEMON_ENV`]. Only `unix://` is
    /// supported until remote daemons have authentication.
    #[arg(
        long,
        global = true,
        value_name = "URL",
        conflicts_with = "socket",
        help = "URL of alchemyd; only unix:///path/to/alchemyd.sock for now [env: ALCHEMY_DAEMON]"
    )]
    pub daemon: Option<String>,

    /// Path of the alchemyd IPC socket, overriding `ALCHEMY_SOCKET`.
    /// Short for `--daemon unix://PATH`.
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Path of the alchemyd IPC socket, short for --daemon unix://PATH [env: ALCHEMY_SOCKET]"
    )]
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
    Status(commands::status::Args),
}

/// Run `cli`, writing its output to `out`.
///
/// `status --watch` only returns on an error, such as the daemon
/// stopping; the caller ends it by dropping the future.
pub async fn run<W: Write>(cli: Cli, out: &mut W) -> Result<(), ClientError> {
    let Target::Unix(socket) = target::resolve(Sources::from_env(cli.daemon, cli.socket))?;
    let context =
        |command| Context::new(IpcDaemon::new(socket), Output::new(out, cli.json), command);
    match cli.command {
        Command::Status(args) => status::run(&mut context(status::NAME), args).await,
    }
}
