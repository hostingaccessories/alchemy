//! The macOS application firewall: an allowed entry for the alchemyd
//! next to the helper, made with `socketfilterfw`.
//!
//! `socketfilterfw` exits 0 whatever happens, so every change is
//! confirmed by reading `--listapps` back.

use std::path::Path;

use super::{ProxyPorts, exec, failed, find_program};
use crate::sibling;
use crate::system::{CommandOutput, System};
use crate::{HelperError, Platform};

/// The application firewall's command-line tool.
pub const SOCKETFILTERFW: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";

/// Add alchemyd and allow its incoming connections, while the firewall
/// is on.
pub(super) fn open<S: System>(system: &S, ports: ProxyPorts) -> Result<bool, HelperError> {
    let no_firewall = || HelperError::NoFirewall {
        platform: Platform::MacOs,
        ports,
    };
    let program = find(system)?.ok_or_else(no_firewall)?;
    let args = ["--getglobalstate"];
    let state = exec(system, program, &args)?;
    match global_state(&state) {
        Some(0) => return Err(no_firewall()),
        Some(_) => {}
        None => return Err(failed(program, &args, &state)),
    }
    let (path, _, _) = sibling::open(system)?;
    let target = argument(&path)?;
    let entry = listed(system, program, target)?;
    if entry == Some(true) {
        return Ok(false);
    }
    let mut outputs = Vec::new();
    if entry.is_none() {
        outputs.push(exec(system, program, &["--add", target])?);
    }
    let unblock = ["--unblockapp", target];
    outputs.push(exec(system, program, &unblock)?);
    if listed(system, program, target)? != Some(true) {
        return Err(unconfirmed(program, &unblock, &outputs));
    }
    Ok(true)
}

/// Remove alchemyd's entry, whether or not the firewall is on.
pub(super) fn close<S: System>(system: &S) -> Result<bool, HelperError> {
    let Some(program) = find(system)? else {
        return Ok(false);
    };
    let path = sibling::path(system)?;
    let target = argument(&path)?;
    if listed(system, program, target)?.is_none() {
        return Ok(false);
    }
    let remove = ["--remove", target];
    let output = exec(system, program, &remove)?;
    if listed(system, program, target)?.is_some() {
        return Err(unconfirmed(program, &remove, &[output]));
    }
    Ok(true)
}

fn find<S: System>(system: &S) -> Result<Option<&'static str>, HelperError> {
    find_program(system, &[SOCKETFILTERFW])
}

/// `path` as a `socketfilterfw` argument: UTF-8 with no control
/// characters, so it matches a `--listapps` line exactly.
fn argument(path: &Path) -> Result<&str, HelperError> {
    match path.to_str() {
        Some(s) if !s.chars().any(char::is_control) => Ok(s),
        _ => Err(HelperError::Refused {
            path: path.to_path_buf(),
            why: "its path is not printable UTF-8",
        }),
    }
}

/// `target`'s entry in the firewall: `Some(true)` when allowed,
/// `Some(false)` when blocked, `None` when absent.
fn listed<S: System>(
    system: &S,
    program: &'static str,
    target: &str,
) -> Result<Option<bool>, HelperError> {
    let output = exec(system, program, &["--listapps"])?;
    Ok(apps(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .find(|(path, _)| path == target)
        .map(|(_, allowed)| allowed))
}

/// The state number in `--getglobalstate` output, such as `1` in
/// `Firewall is enabled. (State = 1)`. 0 means off.
pub fn global_state(output: &CommandOutput) -> Option<u8> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (_, rest) = stdout.split_once("(State = ")?;
    let (state, _) = rest.split_once(')')?;
    state.parse().ok()
}

/// Each app in `--listapps` output and whether it is allowed. Entries
/// look like `1 : /usr/sbin/cupsd` followed by a line holding
/// `(Allow incoming connections)` or `(Block incoming connections)`.
pub fn apps(listing: &str) -> Vec<(String, bool)> {
    let mut apps = Vec::new();
    let mut current: Option<String> = None;
    for line in listing.lines() {
        let line = line.trim();
        if let Some((index, path)) = line.split_once(" : ")
            && !index.is_empty()
            && index.bytes().all(|b| b.is_ascii_digit())
        {
            current = Some(path.trim().to_owned());
        } else if let Some(path) = current.take() {
            if line.contains("Allow incoming connections") {
                apps.push((path, true));
            } else if line.contains("Block incoming connections") {
                apps.push((path, false));
            } else {
                current = Some(path);
            }
        }
    }
    apps
}

/// A change `socketfilterfw` reported nothing about but didn't make, as
/// when a configuration profile manages the firewall.
fn unconfirmed(program: &'static str, args: &[&str], outputs: &[CommandOutput]) -> HelperError {
    let mut said = String::new();
    for output in outputs {
        said.push_str(&String::from_utf8_lossy(&output.stdout));
        said.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    let said = said.trim();
    HelperError::CommandFailed {
        command: std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" "),
        status: "exit 0".to_owned(),
        stderr: if said.is_empty() {
            "alchemyd's --listapps entry did not change".to_owned()
        } else {
            format!("alchemyd's --listapps entry did not change: {said}")
        },
    }
}
