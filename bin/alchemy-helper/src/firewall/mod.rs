//! `open-firewall` and `close-firewall`: Alchemy's ports in the host
//! firewall, opened with one prompt and closed exactly.
//!
//! The ports are 80/tcp, the proxy range (8100-8999/tcp by default),
//! and 5353/udp. On Linux they go into ufw as the app profile `alchemy`
//! and into firewalld as the service `alchemy`, each only while that
//! firewall is active, so closing removes one named unit. Raw nftables
//! and iptables rules are never edited. On macOS the application
//! firewall gets an allowed entry for the alchemyd next to the helper.

mod firewalld;
mod macos;
mod ufw;

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

use crate::command::{exec, failed, must};
use crate::sibling::os;
use crate::system::{OpenError, System};
use crate::{HelperError, Outcome, Platform};

/// The lowest port a [`ProxyPorts`] range may start at, so the firewall
/// operations can never open SSH or another system port. 80/tcp is
/// opened separately.
pub const MIN_PROXY_PORT: u16 = 1024;

/// The name of the ufw app profile and the firewalld service.
pub const NAME: &str = "alchemy";

/// The TCP range apps' own addresses come from, such as `8100-8999`.
///
/// On the command line it is always decimal `START-END`, even for one
/// port, with no sign, leading zero, or whitespace, and it never starts
/// below [`MIN_PROXY_PORT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyPorts {
    start: u16,
    end: u16,
}

/// Why a `--proxy-ports` value was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ProxyPortsError {
    /// Not two decimal ports joined by `-`.
    #[error("expected START-END, such as 8100-8999")]
    Format,
    /// The range starts below [`MIN_PROXY_PORT`].
    #[error("the range can't start below {MIN_PROXY_PORT}, but starts at {0}")]
    Privileged(u16),
    /// The range ends before it starts.
    #[error("the range ends at {end}, before it starts at {start}")]
    Reversed {
        /// The first port.
        start: u16,
        /// The last port.
        end: u16,
    },
}

impl ProxyPorts {
    /// The default range, 8100-8999.
    pub const DEFAULT: Self = Self {
        start: 8100,
        end: 8999,
    };

    /// The ports from `start` to `end`, inclusive.
    pub fn new(start: u16, end: u16) -> Result<Self, ProxyPortsError> {
        if start < MIN_PROXY_PORT {
            return Err(ProxyPortsError::Privileged(start));
        }
        if end < start {
            return Err(ProxyPortsError::Reversed { start, end });
        }
        Ok(Self { start, end })
    }

    /// The first port.
    pub fn start(&self) -> u16 {
        self.start
    }

    /// The last port.
    pub fn end(&self) -> u16 {
        self.end
    }

    /// The range as ufw writes it: `8100:8999`, or `8100` for one port,
    /// since ufw rejects `8100:8100`.
    pub fn ufw(&self) -> String {
        self.joined(':')
    }

    /// The range as firewalld writes it: `8100-8999`, or `8100`.
    pub fn firewalld(&self) -> String {
        self.joined('-')
    }

    fn joined(self, separator: char) -> String {
        if self.start == self.end {
            self.start.to_string()
        } else {
            format!("{}{separator}{}", self.start, self.end)
        }
    }
}

impl Default for ProxyPorts {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl fmt::Display for ProxyPorts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

impl FromStr for ProxyPorts {
    type Err = ProxyPortsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (start, end) = s.split_once('-').ok_or(ProxyPortsError::Format)?;
        Self::new(port(start)?, port(end)?)
    }
}

/// One decimal port in canonical form.
fn port(s: &str) -> Result<u16, ProxyPortsError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ProxyPortsError::Format);
    }
    let port: u16 = s.parse().map_err(|_| ProxyPortsError::Format)?;
    if port.to_string() != s {
        return Err(ProxyPortsError::Format);
    }
    Ok(port)
}

/// Open Alchemy's ports in every active firewall the helper manages.
pub(crate) fn open<S: System>(
    system: &S,
    platform: Platform,
    ports: ProxyPorts,
) -> Result<Outcome, HelperError> {
    let changed = match platform {
        Platform::Linux => {
            let ufw = ufw::find(system)?;
            let firewalld = firewalld::find(system)?;
            let ufw_active = match ufw {
                Some(program) => ufw::active(system, program)?,
                None => false,
            };
            let firewalld_running = match firewalld {
                Some(program) => firewalld::running(system, program)?,
                None => false,
            };
            if !ufw_active && !firewalld_running {
                return Err(HelperError::NoFirewall { platform, ports });
            }
            let mut changed = false;
            if let (Some(program), true) = (ufw, ufw_active) {
                changed |= ufw::open(system, program, ports)?;
            }
            if let (Some(program), true) = (firewalld, firewalld_running) {
                changed |= firewalld::open(system, program, ports)?;
            }
            changed
        }
        Platform::MacOs => macos::open(system, ports)?,
    };
    Ok(Outcome { changed })
}

/// Remove whatever [`open`] added, from every firewall that has it,
/// whether or not that firewall is active now. The range is not needed:
/// each firewall's entry is removed whole.
pub(crate) fn close<S: System>(
    system: &S,
    platform: Platform,
    _ports: ProxyPorts,
) -> Result<Outcome, HelperError> {
    let changed = match platform {
        Platform::Linux => {
            let mut changed = false;
            if let Some(program) = ufw::find(system)? {
                changed |= ufw::close(system, program)?;
            }
            if let Some(program) = firewalld::find(system)? {
                changed |= firewalld::close(system, program)?;
            }
            changed
        }
        Platform::MacOs => macos::close(system)?,
    };
    Ok(Outcome { changed })
}

/// The first of `candidates` that exists as a regular file owned by root
/// that neither group nor others can write.
///
/// A symlink is skipped rather than followed, so a candidate is only ever
/// the file itself. One that exists but fails the ownership check is
/// refused outright.
fn find_program<S: System>(
    system: &S,
    candidates: &[&'static str],
) -> Result<Option<&'static str>, HelperError> {
    for &candidate in candidates {
        let path = std::path::Path::new(candidate);
        let file = match system.open_nofollow(path) {
            Ok(file) => file,
            Err(OpenError::NotFound | OpenError::Symlink) => continue,
            Err(OpenError::Io(source)) => return Err(os("open", path, source)),
        };
        let info = system
            .inspect(&file)
            .map_err(|source| os("inspect", path, source))?;
        let why = if !info.regular {
            "it is not a regular file"
        } else if info.uid != 0 {
            "it is not owned by root"
        } else if info.mode & 0o022 != 0 {
            "group or others can write it"
        } else {
            return Ok(Some(candidate));
        };
        return Err(HelperError::Untrusted {
            path: path.to_path_buf(),
            why,
        });
    }
    Ok(None)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests;
