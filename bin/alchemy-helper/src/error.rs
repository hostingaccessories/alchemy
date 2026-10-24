//! [`HelperError`] and the [`Exit`] statuses it maps to.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::Platform;
use crate::firewall::ProxyPorts;

/// The process exit statuses `alchemy-helper` uses, from `sysexits.h`.
/// Each is a contract that alchemyd branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Exit {
    /// The operation succeeded, or there was nothing to do.
    Success,
    /// The command line was invalid: `EX_USAGE`.
    Usage,
    /// The target exists but is not something the helper will change,
    /// such as a symlink: `EX_DATAERR`.
    DataErr,
    /// The target does not exist: `EX_NOINPUT`.
    NoInput,
    /// The user the operation names does not exist: `EX_NOUSER`.
    NoUser,
    /// The operation cannot work on this system: `EX_UNAVAILABLE`.
    Unavailable,
    /// A fixed system command the operation runs failed: `EX_SOFTWARE`.
    Software,
    /// A system call failed: `EX_OSERR`.
    OsErr,
    /// Writing the result failed: `EX_IOERR`.
    IoErr,
    /// The helper is not running as root: `EX_NOPERM`.
    NoPerm,
}

impl Exit {
    /// The numeric process exit code.
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Usage => 64,
            Self::DataErr => 65,
            Self::NoInput => 66,
            Self::NoUser => 67,
            Self::Unavailable => 69,
            Self::Software => 70,
            Self::OsErr => 71,
            Self::IoErr => 74,
            Self::NoPerm => 77,
        }
    }
}

/// Why an operation failed.
///
/// [`kind`](Self::kind) is the stable token in the result line and
/// [`exit`](Self::exit) the process status; the `Display` form is for
/// people and may change.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HelperError {
    /// The command line was invalid. Holds clap's rendered message.
    #[error("{0}")]
    Usage(String),
    /// The effective UID is not 0.
    #[error("must be run as root")]
    NotRoot,
    /// The operation does not exist on this platform.
    #[error("{operation} is not supported on {platform}: {why}")]
    UnsupportedPlatform {
        /// The subcommand name.
        operation: &'static str,
        /// Where it was run.
        platform: Platform,
        /// Why there is nothing to do here.
        why: &'static str,
    },
    /// The target is on a filesystem where the change would have no
    /// effect.
    #[error("{path}: {why}")]
    Ineffective {
        /// The target.
        path: PathBuf,
        /// What makes the change useless.
        why: &'static str,
    },
    /// The target does not exist.
    #[error("{0}: no such file")]
    NotFound(PathBuf),
    /// The target exists, but the helper refuses to change it.
    #[error("{path}: refusing to change it: {why}")]
    Refused {
        /// The target.
        path: PathBuf,
        /// What is wrong with it.
        why: &'static str,
    },
    /// No user has the UID the operation names.
    #[error("no user has uid {0}")]
    NoSuchUser(u32),
    /// The user exists, but the helper refuses to act for it.
    #[error("user {uid}: refusing to act for it: {why}")]
    RefusedUser {
        /// The UID from the command line.
        uid: u32,
        /// What is wrong with it.
        why: &'static str,
    },
    /// A program the helper would run is not a root-owned regular file
    /// that only root can write.
    #[error("{path}: refusing to run it: {why}")]
    Untrusted {
        /// The program.
        path: PathBuf,
        /// What is wrong with it.
        why: &'static str,
    },
    /// No firewall the helper manages is on, so there is nothing to
    /// open. Raw nftables and iptables rules are never edited.
    #[error("{}", no_firewall(*platform, *ports))]
    NoFirewall {
        /// Where it was run.
        platform: Platform,
        /// The proxy range that was asked for.
        ports: ProxyPorts,
    },
    /// A firewall holds Alchemy's rules but is not running, so they
    /// cannot be removed safely.
    #[error(
        "{path}: {manager} is not running, so its rules for Alchemy cannot be removed; start it \
         and try again"
    )]
    FirewallStopped {
        /// The firewall, such as `firewalld`.
        manager: &'static str,
        /// The file the helper added to it.
        path: PathBuf,
    },
    /// A fixed system command exited unsuccessfully.
    #[error("`{command}` failed ({status}): {stderr}")]
    CommandFailed {
        /// The program and its arguments, space-separated.
        command: String,
        /// How it ended, such as `exit 1`.
        status: String,
        /// Its stderr, trimmed.
        stderr: String,
    },
    /// A system call failed.
    #[error("{path}: cannot {action}: {source}")]
    Os {
        /// What the helper was doing, as a verb phrase.
        action: &'static str,
        /// The file it was acting on.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}

impl HelperError {
    /// The stable machine-readable token for this error.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Usage(_) => "usage",
            Self::NotRoot => "not_root",
            Self::UnsupportedPlatform { .. }
            | Self::Ineffective { .. }
            | Self::FirewallStopped { .. } => "unsupported",
            Self::NotFound(_) => "not_found",
            Self::Refused { .. } | Self::RefusedUser { .. } | Self::Untrusted { .. } => "refused",
            Self::NoFirewall { .. } => "no_firewall",
            Self::NoSuchUser(_) => "no_such_user",
            Self::CommandFailed { .. } => "command_failed",
            Self::Os { .. } => "os_error",
        }
    }

    /// The process exit status for this error.
    pub const fn exit(&self) -> Exit {
        match self {
            Self::Usage(_) => Exit::Usage,
            Self::NotRoot => Exit::NoPerm,
            Self::UnsupportedPlatform { .. }
            | Self::Ineffective { .. }
            | Self::NoFirewall { .. }
            | Self::FirewallStopped { .. } => Exit::Unavailable,
            Self::NotFound(_) => Exit::NoInput,
            Self::Refused { .. } | Self::RefusedUser { .. } | Self::Untrusted { .. } => {
                Exit::DataErr
            }
            Self::NoSuchUser(_) => Exit::NoUser,
            Self::CommandFailed { .. } => Exit::Software,
            Self::Os { .. } => Exit::OsErr,
        }
    }
}

fn no_firewall(platform: Platform, ports: ProxyPorts) -> String {
    match platform {
        Platform::Linux => format!(
            "neither ufw nor firewalld is active, and Alchemy never edits raw nftables or \
             iptables rules: if a firewall blocks Alchemy, allow 80/tcp, {}/tcp, and 5353/udp \
             by hand",
            ports.firewalld()
        ),
        Platform::MacOs => "the macOS application firewall is off, so it blocks nothing".to_owned(),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_exit_codes() {
        let path = || PathBuf::from("/opt/alchemy/alchemyd");
        let cases = [
            (HelperError::Usage(String::new()), "usage", 64),
            (HelperError::NotRoot, "not_root", 77),
            (
                HelperError::UnsupportedPlatform {
                    operation: "grant-low-ports",
                    platform: Platform::MacOs,
                    why: "",
                },
                "unsupported",
                69,
            ),
            (
                HelperError::Ineffective {
                    path: path(),
                    why: "",
                },
                "unsupported",
                69,
            ),
            (HelperError::NotFound(path()), "not_found", 66),
            (
                HelperError::Refused {
                    path: path(),
                    why: "",
                },
                "refused",
                65,
            ),
            (
                HelperError::RefusedUser { uid: 1000, why: "" },
                "refused",
                65,
            ),
            (HelperError::NoSuchUser(1000), "no_such_user", 67),
            (
                HelperError::Untrusted {
                    path: path(),
                    why: "",
                },
                "refused",
                65,
            ),
            (
                HelperError::NoFirewall {
                    platform: Platform::Linux,
                    ports: ProxyPorts::DEFAULT,
                },
                "no_firewall",
                69,
            ),
            (
                HelperError::FirewallStopped {
                    manager: "firewalld",
                    path: path(),
                },
                "unsupported",
                69,
            ),
            (
                HelperError::CommandFailed {
                    command: String::new(),
                    status: String::new(),
                    stderr: String::new(),
                },
                "command_failed",
                70,
            ),
            (
                HelperError::Os {
                    action: "",
                    path: path(),
                    source: io::Error::other("boom"),
                },
                "os_error",
                71,
            ),
        ];
        for (err, kind, code) in cases {
            assert_eq!(err.kind(), kind, "{err:?}");
            assert_eq!(err.exit().code(), code, "{err:?}");
        }
    }

    #[test]
    fn firewall_messages() {
        let cases = [
            (
                Platform::Linux,
                "neither ufw nor firewalld is active, and Alchemy never edits raw nftables or \
                 iptables rules: if a firewall blocks Alchemy, allow 80/tcp, 8100-8999/tcp, and \
                 5353/udp by hand",
            ),
            (
                Platform::MacOs,
                "the macOS application firewall is off, so it blocks nothing",
            ),
        ];
        for (platform, want) in cases {
            let err = HelperError::NoFirewall {
                platform,
                ports: ProxyPorts::DEFAULT,
            };
            assert_eq!(err.to_string(), want);
        }
    }

    #[test]
    fn exit_codes_are_sysexits() {
        let cases = [
            (Exit::Success, 0),
            (Exit::Usage, 64),
            (Exit::DataErr, 65),
            (Exit::NoInput, 66),
            (Exit::NoUser, 67),
            (Exit::Unavailable, 69),
            (Exit::Software, 70),
            (Exit::OsErr, 71),
            (Exit::IoErr, 74),
            (Exit::NoPerm, 77),
        ];
        for (exit, code) in cases {
            assert_eq!(exit.code(), code, "{exit:?}");
        }
    }
}
