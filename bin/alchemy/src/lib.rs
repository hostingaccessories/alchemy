//! The `alchemy` CLI.
//!
//! Each subcommand maps to HTTP requests against `alchemyd`'s API on its
//! Unix socket. The CLI holds no state of its own.

pub mod browser;
pub mod commands;
mod context;
mod daemon;
pub mod editor;
mod error;
mod output;
pub mod prompt;
pub mod safety;
mod sse;
pub mod sudo;
mod target;

use std::ffi::OsStr;
use std::io::Write;
use std::path::PathBuf;

use alchemy_api::operations::Action;
use clap::{Parser, Subcommand};

use crate::commands::{app, completions, generate_man, machine, open, settings, status, version};
use crate::context::Context;
use crate::daemon::HttpDaemon;
use crate::editor::TerminalEditor;
use crate::output::Output;
use crate::prompt::{Prompt, TtyPrompt};
use crate::sudo::{Sudo, TerminalSudo};
use crate::target::{Sources, Target};

pub use error::{ClientError, Exit, ProtocolError};
pub use output::Color;
pub use target::DAEMON_ENV;

/// Manage Alchemy from the command line. clap shows the package
/// description instead of this, so keep the two in step.
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

    /// Path of the alchemyd socket, overriding `ALCHEMY_SOCKET`.
    /// Short for `--daemon unix://PATH`.
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Path of the alchemyd socket, short for --daemon unix://PATH [env: ALCHEMY_SOCKET]"
    )]
    pub socket: Option<PathBuf>,

    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show whether alchemyd is reachable, which version it runs, whether
    /// Docker is ready, and whether its networks overlap the local network.
    Status(commands::status::Args),

    /// Work with apps: the Compose projects Alchemy runs, found in the app
    /// folders.
    App(commands::app::Args),

    /// Work with the machines Alchemy runs apps on. For now, only this
    /// one.
    Machine(commands::machine::Args),

    /// Show and change alchemyd's settings, such as where it looks for
    /// apps and whether it announces their .local names.
    Settings(commands::settings::Args),

    /// Show the versions of alchemy and alchemyd, and the protocol each
    /// speaks.
    Version(commands::version::Args),

    /// Open Alchemy's web UI in your browser, signed in.
    ///
    /// alchemyd's web UI only answers a browser that has signed in. This
    /// asks alchemyd, over its socket, for a link that signs the browser
    /// in, and opens it. The link works once, within a minute. With
    /// --print, or when no browser can be opened, it prints the link
    /// instead, to open by hand or on the other end of an SSH tunnel.
    Open(commands::open::Args),

    /// Write a shell completion script to stdout.
    Completions(commands::completions::Args),

    /// Write man pages for packaging.
    #[command(hide = true)]
    GenerateMan(commands::generate_man::Args),

    /// A stand-in destructive command for the e2e tests.
    #[cfg(feature = "test-command")]
    #[command(hide = true)]
    SafetyProbe(commands::safety_probe::Args),
}

/// Run `cli`, writing its results to `out`, styled only when `color`
/// allows it, and asking for any confirmation on the terminal. The
/// caller reports a returned error with [`report`].
///
/// `status --watch` only returns on an error, such as the daemon
/// stopping; the caller ends it by dropping the future.
pub async fn run<W: Write>(cli: Cli, out: &mut W, color: Color) -> Result<(), ClientError> {
    run_with(cli, out, color, &mut TtyPrompt).await
}

/// [`run`], asking for confirmation on `prompt` instead of the terminal.
pub async fn run_with<W: Write, P: Prompt>(
    cli: Cli,
    out: &mut W,
    color: Color,
    prompt: &mut P,
) -> Result<(), ClientError> {
    run_with_sudo(cli, out, color, prompt, &mut TerminalSudo::from_env()).await
}

/// [`run_with`], running the `sudo` commands alchemyd hands over through
/// `sudo` instead of on the terminal.
pub async fn run_with_sudo<W: Write, P: Prompt, S: Sudo>(
    cli: Cli,
    out: &mut W,
    color: Color,
    prompt: &mut P,
    sudo: &mut S,
) -> Result<(), ClientError> {
    let Cli {
        json,
        daemon,
        socket,
        command,
    } = cli;
    let context = |out, command| -> Result<_, ClientError> {
        let Target::Unix(socket) = target::resolve(Sources::from_env(daemon, socket))?;
        Ok(Context::new(
            HttpDaemon::new(socket),
            Output::new(out, json, color),
            command,
        ))
    };
    match command {
        Command::Status(args) => status::run(&mut context(out, status::NAME)?, args).await,
        Command::Version(args) => version::run(&mut context(out, version::NAME)?, args).await,
        Command::Open(args) => open::run_system(&mut context(out, open::NAME)?, args).await,
        Command::App(args) => match args.command {
            app::Verb::List(args) => {
                app::list::run(&mut context(out, app::list::NAME)?, args).await
            }
            app::Verb::Show(args) => {
                app::show::run(&mut context(out, app::show::NAME)?, args).await
            }
            app::Verb::Logs(args) => {
                app::logs::run(&mut context(out, app::logs::NAME)?, args).await
            }
            app::Verb::Create(args) => {
                let mut context = context(out, app::create::NAME)?;
                app::create::run(&mut context, prompt, args).await
            }
            app::Verb::Edit(args) => {
                let mut context = context(out, app::edit::NAME)?;
                app::edit::run(&mut context, prompt, &mut TerminalEditor, args).await
            }
            app::Verb::Stats(args) => {
                let mut context = context(out, app::stats::NAME)?;
                app::stats::run(&mut context, args).await
            }
            app::Verb::Up(args) => {
                let mut context = context(out, app::action::name(Action::Up))?;
                app::action::run(&mut context, prompt, Action::Up, args).await
            }
            app::Verb::Down(args) => {
                let mut context = context(out, app::action::name(Action::Down))?;
                app::action::run(&mut context, prompt, Action::Down, args).await
            }
            app::Verb::Start(args) => {
                let mut context = context(out, app::action::name(Action::Start))?;
                app::action::run(&mut context, prompt, Action::Start, args).await
            }
            app::Verb::Stop(args) => {
                let mut context = context(out, app::action::name(Action::Stop))?;
                app::action::run(&mut context, prompt, Action::Stop, args).await
            }
            app::Verb::Restart(args) => {
                let mut context = context(out, app::action::name(Action::Restart))?;
                app::action::run(&mut context, prompt, Action::Restart, args).await
            }
            app::Verb::Pull(args) => {
                let mut context = context(out, app::action::name(Action::Pull))?;
                app::action::run(&mut context, prompt, Action::Pull, args).await
            }
            app::Verb::Build(args) => {
                let mut context = context(out, app::action::name(Action::Build))?;
                app::action::run(&mut context, prompt, Action::Build, args).await
            }
            app::Verb::Update(args) => {
                let mut context = context(out, app::action::name(Action::Update))?;
                app::action::run(&mut context, prompt, Action::Update, args).await
            }
            app::Verb::Cancel(args) => {
                app::cancel::run(&mut context(out, app::cancel::NAME)?, args).await
            }
        },
        Command::Machine(args) => run_machine(args.command, sudo, |name| context(out, name)).await,
        Command::Settings(args) => match args.command {
            settings::Verb::Show(args) => {
                settings::show::run(&mut context(out, settings::show::NAME)?, args).await
            }
            settings::Verb::Set(args) => {
                settings::set::run(&mut context(out, settings::set::NAME)?, args).await
            }
        },
        Command::Completions(args) => completions::run(out, &args),
        Command::GenerateMan(args) => generate_man::run(&args),
        #[cfg(feature = "test-command")]
        Command::SafetyProbe(args) => {
            let mut context = context(out, commands::safety_probe::NAME)?;
            commands::safety_probe::run(&mut context, prompt, args).await
        }
    }
}

/// Run the `alchemy machine` verb `verb` in the context `context` makes
/// for the command named, running any `sudo` command through `sudo`.
async fn run_machine<W: Write, S: Sudo>(
    verb: machine::Verb,
    sudo: &mut S,
    context: impl FnOnce(&'static str) -> Result<Context<HttpDaemon, W>, ClientError>,
) -> Result<(), ClientError> {
    match verb {
        machine::Verb::Privileges(args) => {
            let mut context = context(machine::privileges::name(&args))?;
            machine::privileges::run(&mut context, sudo, args).await
        }
        machine::Verb::Stats(args) => {
            machine::stats::run(&mut context(machine::stats::NAME)?, args).await
        }
        machine::Verb::Ports(args) => {
            machine::ports::run(&mut context(machine::ports::NAME)?, args).await
        }
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
