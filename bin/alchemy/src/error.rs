//! CLI error type and exit-code mapping.

use alchemy_ipc::{IpcError, Response};
use thiserror::Error;

/// The CLI was invoked or configured wrongly. Clap exits with it too.
const EXIT_USAGE: u8 = 2;
/// `sysexits.h` `EX_UNAVAILABLE`.
const EX_UNAVAILABLE: u8 = 69;
/// `sysexits.h` `EX_IOERR`.
const EX_IOERR: u8 = 74;
/// `sysexits.h` `EX_PROTOCOL`.
const EX_PROTOCOL: u8 = 76;

/// Errors surfaced to the CLI user.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientError {
    /// The daemon socket could not be reached.
    #[error("cannot reach alchemyd: {0}")]
    DaemonUnreachable(String),
    /// The daemon closed the connection before answering.
    #[error("alchemyd closed the connection without responding")]
    ConnectionClosed,
    /// The daemon ended a stream the CLI was following, usually because
    /// it stopped.
    #[error("alchemyd closed the connection; is it still running?")]
    Disconnected,
    /// The exchange failed below the message layer.
    #[error(transparent)]
    Ipc(#[from] IpcError),
    /// The daemon answered with an error.
    #[error("alchemyd: {0}")]
    Daemon(String),
    /// The daemon answered with a response this command did not expect.
    #[error("unexpected response from alchemyd: {0}")]
    Unexpected(String),
    /// The daemon predates a request this command needs and answered it
    /// as a bad request.
    #[error(
        "alchemyd {daemon} is too old for `{command}`; upgrade it to match alchemy {cli} and restart it",
        cli = env!("CARGO_PKG_VERSION")
    )]
    DaemonTooOld {
        /// The daemon's reported version.
        daemon: String,
        /// The command that needed the newer daemon, as typed after
        /// `alchemy`.
        command: &'static str,
    },
    /// The daemon target names a remote daemon. Those need
    /// authentication, which arrives in M13, so none is contacted.
    #[error(
        "cannot use a {scheme}:// daemon from {origin}: remote daemons need authentication, which arrives in M13 (https://github.com/hostingaccessories/alchemy/issues/16)\nhint: forward the remote socket over SSH and pass the local end with --socket"
    )]
    RemoteDaemon {
        /// Where the target came from: a flag or an environment variable.
        origin: &'static str,
        /// The URL scheme, lowercased. The rest of the URL is not kept,
        /// since it may carry credentials.
        scheme: String,
    },
    /// The daemon target is malformed.
    #[error("invalid {origin}: {reason}")]
    InvalidTarget {
        /// Where the target came from: a flag or an environment variable.
        origin: &'static str,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Writing the command's output failed.
    #[error("cannot write output: {0}")]
    Output(#[from] std::io::Error),
}

impl ClientError {
    /// Convert an unexpected or error response into a [`ClientError`].
    pub fn from_response(resp: Response) -> Self {
        match resp {
            Response::Error { message, .. } => Self::Daemon(message),
            other => Self::Unexpected(format!("{other:?}")),
        }
    }

    /// The process exit code for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::DaemonUnreachable(_) | Self::Disconnected => EX_UNAVAILABLE,
            Self::ConnectionClosed
            | Self::Ipc(_)
            | Self::Unexpected(_)
            | Self::DaemonTooOld { .. } => EX_PROTOCOL,
            Self::Output(_) => EX_IOERR,
            Self::RemoteDaemon { .. } | Self::InvalidTarget { .. } => EXIT_USAGE,
            Self::Daemon(_) => 1,
        }
    }
}
