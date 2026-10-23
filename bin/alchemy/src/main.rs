//! `alchemy` entry point.

use std::process::ExitCode;

use clap::Parser;

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
    match runtime.block_on(alchemy::run(cli)) {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("alchemy: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}
