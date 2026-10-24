//! firewalld: the service `alchemy`, added to the default zone
//! permanently and at runtime, and removed from every zone that has it.

use std::path::Path;

use super::{NAME, ProxyPorts, exec, failed, find_program, must};
use crate::HelperError;
use crate::sibling::os;
use crate::system::System;

/// Where firewalld reads the service from.
pub const SERVICE: &str = "/etc/firewalld/services/alchemy.xml";

const PROGRAMS: &[&str] = &[
    "/usr/bin/firewall-cmd",
    "/usr/sbin/firewall-cmd",
    "/bin/firewall-cmd",
];

/// `firewall-cmd --state` exits 252 when firewalld is not running.
const NOT_RUNNING: i32 = 252;

/// `firewall-cmd --info-service` exits 101 for a service it hasn't loaded.
const INVALID_SERVICE: i32 = 101;

/// The service definition for `ports`.
pub fn service(ports: ProxyPorts) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <service>\n  \
         <short>Alchemy</short>\n  \
         <description>Alchemy reverse proxy (80), app ports ({0}) and mDNS (5353/udp).</description>\n  \
         <port protocol=\"tcp\" port=\"80\"/>\n  \
         <port protocol=\"tcp\" port=\"{0}\"/>\n  \
         <port protocol=\"udp\" port=\"5353\"/>\n\
         </service>\n",
        ports.firewalld()
    )
}

pub(super) fn find<S: System>(system: &S) -> Result<Option<&'static str>, HelperError> {
    find_program(system, PROGRAMS)
}

pub(super) fn running<S: System>(system: &S, program: &'static str) -> Result<bool, HelperError> {
    let args = ["--state"];
    let output = exec(system, program, &args)?;
    match output.code {
        Some(0) => Ok(true),
        Some(NOT_RUNNING) => Ok(false),
        _ => Err(failed(program, &args, &output)),
    }
}

/// Write the service if it differs, reload so firewalld sees it, and add
/// it to the default zone, permanently and at runtime.
pub(super) fn open<S: System>(
    system: &S,
    program: &'static str,
    ports: ProxyPorts,
) -> Result<bool, HelperError> {
    let path = Path::new(SERVICE);
    let service = service(ports);
    let current = system
        .read_file(path)
        .map_err(|source| os("read", path, source))?;
    let rewritten = current.as_deref() != Some(service.as_bytes());
    if rewritten {
        system
            .write_file(path, service.as_bytes(), 0o644)
            .map_err(|source| os("write", path, source))?;
    }
    let reload = rewritten || !loaded(system, program)?;
    if reload {
        must(system, program, &["--reload"])?;
    }
    let query = format!("--query-service={NAME}");
    let add = format!("--add-service={NAME}");
    let permanent = has(system, program, &["--permanent", &query])?;
    if !permanent {
        must(system, program, &["--permanent", &add])?;
    }
    let runtime = has(system, program, &[&query])?;
    if !runtime {
        must(system, program, &[&add])?;
    }
    Ok(reload || !permanent || !runtime)
}

/// Remove the service from every zone that has it, then delete the file
/// and reload. In that order, because a reload fails while a zone still
/// names a service whose file is gone.
pub(super) fn close<S: System>(system: &S, program: &'static str) -> Result<bool, HelperError> {
    let path = Path::new(SERVICE);
    if !running(system, program)? {
        let exists = system
            .read_file(path)
            .map_err(|source| os("read", path, source))?
            .is_some();
        if exists {
            return Err(HelperError::FirewallStopped {
                manager: "firewalld",
                path: path.to_path_buf(),
            });
        }
        return Ok(false);
    }
    let zones = must(system, program, &["--permanent", "--get-zones"])?;
    let zones = String::from_utf8_lossy(&zones.stdout).into_owned();
    let query = format!("--query-service={NAME}");
    let remove = format!("--remove-service={NAME}");
    let mut removed = false;
    for zone in zones.split_whitespace() {
        let zone = format!("--zone={zone}");
        if has(system, program, &["--permanent", &zone, &query])? {
            must(system, program, &["--permanent", &zone, &remove])?;
            removed = true;
        }
    }
    let deleted = system
        .remove_file(path)
        .map_err(|source| os("remove", path, source))?;
    if removed || deleted {
        must(system, program, &["--reload"])?;
    }
    Ok(removed || deleted)
}

/// Whether firewalld has loaded the service: a file written without a
/// reload stays invisible to it.
fn loaded<S: System>(system: &S, program: &'static str) -> Result<bool, HelperError> {
    let info = format!("--info-service={NAME}");
    let args = [info.as_str()];
    let output = exec(system, program, &args)?;
    match output.code {
        Some(0) => Ok(true),
        Some(INVALID_SERVICE) => Ok(false),
        _ => Err(failed(program, &args, &output)),
    }
}

/// A `--query-service` answer: exit 0 for yes, 1 for no.
fn has<S: System>(system: &S, program: &'static str, args: &[&str]) -> Result<bool, HelperError> {
    let output = exec(system, program, args)?;
    match output.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failed(program, args, &output)),
    }
}
