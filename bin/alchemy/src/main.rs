//! `alchemy` entry point.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::process::ExitCode;

use alchemy::{Cli, ClientError, Color};
use clap::Parser;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) if !e.use_stderr() => {
            let _ = e.print();
            return ExitCode::SUCCESS;
        }
        Err(e) => return fail(&ClientError::from_clap(&e), alchemy::wants_json(&args)),
    };
    let json = cli.json;
    let color = Color::detect(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
    );
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail(&ClientError::Runtime(e), json),
    };
    let mut stdout = std::io::stdout();
    let outcome = runtime.block_on(async {
        tokio::select! {
            result = alchemy::run(cli, &mut stdout, color) => result,
            Ok(()) = tokio::signal::ctrl_c() => Err(ClientError::Interrupted),
        }
    });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e, json),
    }
}

fn fail(err: &ClientError, json: bool) -> ExitCode {
    ExitCode::from(alchemy::report(err, json, &mut std::io::stderr().lock()))
}
