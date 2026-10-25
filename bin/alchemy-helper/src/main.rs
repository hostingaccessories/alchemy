//! `alchemy-helper` entry point: parse, check for root, run, report.

use std::process::ExitCode;

use alchemy_helper::{Exit, HelperError, Platform, RealSystem};

fn main() -> ExitCode {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    let operation = match alchemy_helper::parse(std::env::args_os()) {
        Ok(operation) => operation,
        Err(e) if !e.is_usage() => {
            print!("{e}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            let result = Err(HelperError::Usage(e.to_string()));
            return exit(alchemy_helper::report(
                None,
                &result,
                &mut stdout,
                &mut stderr,
            ));
        }
    };
    let result = if rustix::process::geteuid().is_root() {
        alchemy_helper::run(&operation, &RealSystem, Platform::current())
    } else {
        Err(HelperError::NotRoot)
    };
    exit(alchemy_helper::report(
        Some(&operation),
        &result,
        &mut stdout,
        &mut stderr,
    ))
}

fn exit(status: Exit) -> ExitCode {
    ExitCode::from(status.code())
}
