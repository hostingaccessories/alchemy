//! [`HelperError`] and the [`Exit`] statuses it maps to.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::Platform;

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
    /// The operation cannot work on this system: `EX_UNAVAILABLE`.
    Unavailable,
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
            Self::Unavailable => 69,
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
            Self::UnsupportedPlatform { .. } | Self::Ineffective { .. } => "unsupported",
            Self::NotFound(_) => "not_found",
            Self::Refused { .. } => "refused",
            Self::Os { .. } => "os_error",
        }
    }

    /// The process exit status for this error.
    pub const fn exit(&self) -> Exit {
        match self {
            Self::Usage(_) => Exit::Usage,
            Self::NotRoot => Exit::NoPerm,
            Self::UnsupportedPlatform { .. } | Self::Ineffective { .. } => Exit::Unavailable,
            Self::NotFound(_) => Exit::NoInput,
            Self::Refused { .. } => Exit::DataErr,
            Self::Os { .. } => Exit::OsErr,
        }
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
    fn exit_codes_are_sysexits() {
        let cases = [
            (Exit::Success, 0),
            (Exit::Usage, 64),
            (Exit::DataErr, 65),
            (Exit::NoInput, 66),
            (Exit::Unavailable, 69),
            (Exit::OsErr, 71),
            (Exit::IoErr, 74),
            (Exit::NoPerm, 77),
        ];
        for (exit, code) in cases {
            assert_eq!(exit.code(), code, "{exit:?}");
        }
    }
}
