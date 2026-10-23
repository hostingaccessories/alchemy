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

use std::ffi::OsStr;
use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::commands::status;
use crate::context::Context;
use crate::daemon::IpcDaemon;
use crate::output::Output;
use crate::target::{Sources, Target};

pub use error::{ClientError, Exit};
pub use output::Color;
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

/// Run `cli`, writing its results to `out`, styled only when `color`
/// allows it. The caller reports a returned error with [`report`].
///
/// `status --watch` only returns on an error, such as the daemon
/// stopping; the caller ends it by dropping the future.
pub async fn run<W: Write>(cli: Cli, out: &mut W, color: Color) -> Result<(), ClientError> {
    let Target::Unix(socket) = target::resolve(Sources::from_env(cli.daemon, cli.socket))?;
    let context = |command| {
        Context::new(
            IpcDaemon::new(socket),
            Output::new(out, cli.json, color),
            command,
        )
    };
    match cli.command {
        Command::Status(args) => status::run(&mut context(status::NAME), args).await,
    }
}

/// Write `err` to `stderr`, as one JSON document when `json` is set and
/// as text otherwise, and return the process exit code.
///
/// Quiet errors, such as an interrupt, are not written. Failing to write
/// the report is ignored: stderr is the last place left to say anything.
pub fn report<E: Write>(err: &ClientError, json: bool, stderr: &mut E) -> u8 {
    if !err.is_quiet() {
        let report = if json { err.to_json() } else { err.to_text() };
        let _ = stderr.write_all(report.as_bytes());
        let _ = stderr.flush();
    }
    err.exit_code()
}

/// Whether the raw command line asks for `--json`, for reporting a
/// command line clap rejected. Arguments after `--` are operands, not
/// flags.
pub fn wants_json<A: AsRef<OsStr>>(args: &[A]) -> bool {
    args.iter()
        .map(AsRef::as_ref)
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json")
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn wants_json_finds_the_flag_before_operands() {
        let cases: [(&[&str], bool); 6] = [
            (&["alchemy", "status"], false),
            (&["alchemy", "--json", "status"], true),
            (&["alchemy", "status", "--json", "--nope"], true),
            (&["alchemy", "status", "--", "--json"], false),
            (&["alchemy", "status", "--jsonx"], false),
            (&[], false),
        ];
        for (args, want) in cases {
            assert_eq!(wants_json(args), want, "{args:?}");
        }
    }

    #[test]
    fn report_writes_text_or_json_and_returns_the_exit_code() {
        let err = ClientError::Daemon("boom".into());
        let cases = [
            (false, "alchemy: alchemyd: boom\n"),
            (
                true,
                "{\"error\":{\"hint\":null,\"kind\":\"daemon\",\"message\":\"alchemyd: boom\"}}\n",
            ),
        ];
        for (json, want) in cases {
            let mut stderr = Vec::new();
            assert_eq!(report(&err, json, &mut stderr), 1);
            assert_eq!(String::from_utf8(stderr).unwrap(), want);
        }
    }

    #[test]
    fn quiet_errors_only_set_the_exit_code() {
        for json in [false, true] {
            let mut stderr = Vec::new();
            assert_eq!(report(&ClientError::Interrupted, json, &mut stderr), 130);
            assert_eq!(String::from_utf8(stderr).unwrap(), "");
        }
    }
}
