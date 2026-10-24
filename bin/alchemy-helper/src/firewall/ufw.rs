//! ufw: the app profile `alchemy`, allowed and deleted as one rule.

use std::path::Path;

use super::{NAME, ProxyPorts, failed, find_program, must};
use crate::HelperError;
use crate::sibling::os;
use crate::system::{CommandOutput, System};

/// Where ufw reads the profile from.
pub const PROFILE: &str = "/etc/ufw/applications.d/alchemy";

const PROGRAMS: &[&str] = &["/usr/sbin/ufw", "/sbin/ufw"];

/// The profile for `ports`.
pub fn profile(ports: ProxyPorts) -> String {
    format!(
        "[{NAME}]\n\
         title=Alchemy\n\
         description=Alchemy reverse proxy (80), app ports ({}) and mDNS (5353/udp)\n\
         ports=80/tcp|{}/tcp|5353/udp\n",
        ports.firewalld(),
        ports.ufw()
    )
}

pub(super) fn find<S: System>(system: &S) -> Result<Option<&'static str>, HelperError> {
    find_program(system, PROGRAMS)
}

/// Whether ufw is enabled. `systemctl is-active ufw` can't tell: the unit
/// is a oneshot that stays active while ufw is disabled.
pub(super) fn active<S: System>(system: &S, program: &'static str) -> Result<bool, HelperError> {
    let args = ["status"];
    let output = must(system, program, &args)?;
    match String::from_utf8_lossy(&output.stdout).lines().next() {
        Some("Status: active") => Ok(true),
        Some("Status: inactive") => Ok(false),
        _ => Err(failed(program, &args, &output)),
    }
}

/// Write the profile if it differs and allow it. A changed profile under
/// an existing rule needs `ufw app update`, or the old ports stay open.
pub(super) fn open<S: System>(
    system: &S,
    program: &'static str,
    ports: ProxyPorts,
) -> Result<bool, HelperError> {
    let path = Path::new(PROFILE);
    let profile = profile(ports);
    let current = system
        .read_file(path)
        .map_err(|source| os("read", path, source))?;
    let rewritten = current.as_deref() != Some(profile.as_bytes());
    if rewritten {
        system
            .write_file(path, profile.as_bytes(), 0o644)
            .map_err(|source| os("write", path, source))?;
    }
    let added = rule_changed(&must(system, program, &["allow", NAME])?);
    if rewritten && !added {
        must(system, program, &["app", "update", NAME])?;
    }
    Ok(rewritten || added)
}

/// Delete the rule, then the profile. Both work while ufw is disabled,
/// and deleting the rule works without the profile.
pub(super) fn close<S: System>(system: &S, program: &'static str) -> Result<bool, HelperError> {
    let deleted = rule_changed(&must(system, program, &["delete", "allow", NAME])?);
    let path = Path::new(PROFILE);
    let removed = system
        .remove_file(path)
        .map_err(|source| os("remove", path, source))?;
    Ok(deleted || removed)
}

/// Whether `ufw allow` or `ufw delete` changed a rule. Both exit 0 either
/// way: a change prints `Rule added`, `Rule deleted`, or, while ufw is
/// disabled, `Rules updated`, and no change prints `Skipping adding
/// existing rule` or `Could not delete non-existent rule`.
fn rule_changed(output: &CommandOutput) -> bool {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.starts_with("Rule"))
}
