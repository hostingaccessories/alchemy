//! `alchemy-helper`: the privileged one-shot.
//!
//! This binary is the security boundary. It is never setuid and never
//! long-running: callers elevate it explicitly (`sudo`, `pkexec`, or
//! macOS `osascript ... with administrator privileges`), it performs one
//! narrowly validated operation, and it exits. It refuses to run unless
//! its effective UID is already 0.
//!
//! Operations are added as subcommands with strictly validated
//! arguments; nothing is ever passed through a shell.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// `sysexits.h` `EX_NOPERM`.
const EX_NOPERM: u8 = 77;

/// Alchemy privileged helper.
#[derive(Debug, Parser)]
#[command(name = "alchemy-helper", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Verify the helper is running with root privileges and exit.
    Check,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if !rustix::process::geteuid().is_root() {
        eprintln!("alchemy-helper: must be run as root");
        return ExitCode::from(EX_NOPERM);
    }
    match cli.command {
        Command::Check => {
            println!("ok");
            ExitCode::SUCCESS
        }
    }
}
