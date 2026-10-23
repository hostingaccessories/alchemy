//! `alchemy` entry point.

use std::process::ExitCode;

use clap::Parser;

/// Shell convention for a process ended by `SIGINT`: 128 + 2.
const EXIT_INTERRUPTED: u8 = 130;

fn main() -> ExitCode {
    let cli = alchemy::Cli::parse();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("alchemy: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdout = std::io::stdout();
    let outcome = runtime.block_on(async {
        tokio::select! {
            result = alchemy::run(cli, &mut stdout) => Some(result),
            Ok(()) = tokio::signal::ctrl_c() => None,
        }
    });
    match outcome {
        Some(Ok(())) => ExitCode::SUCCESS,
        Some(Err(e)) => {
            eprintln!("alchemy: {e}");
            ExitCode::from(e.exit_code())
        }
        None => ExitCode::from(EXIT_INTERRUPTED),
    }
}
