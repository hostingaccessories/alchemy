//! Re-executing alchemyd in place, after a privilege change that only
//! takes effect at exec, such as low ports.
//!
//! The daemon shuts down cleanly first: [`run`](crate::run) returns
//! [`Stopped::Restart`](crate::Stopped::Restart), its listeners, socket,
//! state lock, and token file are gone, and only then does `main` call
//! [`exec_self`]. `execve` keeps the PID, so whatever supervises
//! alchemyd sees no exit. Every file the daemon opened is close-on-exec.
//!
//! A binary with file capabilities is a secure exec: the new process is
//! not dumpable, and the loader drops variables such as `LD_PRELOAD`.
//! `ALCHEMY_*` and other ordinary variables survive.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;

/// What [`exec_self`] runs: the canonical path of the running binary,
/// and the arguments it was started with, `argv[0]` included.
pub fn command() -> io::Result<(PathBuf, Vec<OsString>)> {
    let exe = std::env::current_exe().and_then(std::fs::canonicalize)?;
    Ok((exe, std::env::args_os().collect()))
}

/// Replace this process with a fresh start of its own canonical binary,
/// with the same arguments and environment. Returns only if that fails.
pub fn exec_self() -> io::Error {
    let (exe, args) = match command() {
        Ok(command) => command,
        Err(error) => return error,
    };
    let mut args = args.into_iter();
    let mut command = std::process::Command::new(&exe);
    if let Some(argv0) = args.next() {
        command.arg0(argv0);
    }
    command.args(args).exec()
}
