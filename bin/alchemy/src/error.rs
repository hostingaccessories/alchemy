//! CLI error type, its exit codes, and how it is reported.
//!
//! Every failure becomes a [`ClientError`]. Its text form is
//! `alchemy: <what failed>: <why>` with an optional `hint:` line; with
//! `--json` it is one `{"error":{"kind","message","hint"}}` document.
//! Both go to stderr and carry no ANSI codes.

use alchemy_ipc::{IpcError, Response};
use thiserror::Error;

/// The process exit statuses `alchemy` uses. Each is a contract that
/// scripts branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Exit {
    /// The command did what was asked.
    Success,
    /// The operation failed, for example the daemon refused it.
    Failure,
    /// The command line was invalid.
    Usage,
    /// A confirmation was declined, or was needed without a TTY.
    Confirmation,
    /// alchemyd is unreachable or went away: `sysexits.h` `EX_UNAVAILABLE`.
    Unavailable,
    /// Writing the output failed: `sysexits.h` `EX_IOERR`.
    Io,
    /// alchemy and alchemyd disagree on the protocol, or one is too old
    /// for the other: `sysexits.h` `EX_PROTOCOL`.
    Protocol,
    /// The user interrupted the command: 128 + `SIGINT`.
    Interrupted,
}

impl Exit {
    /// The numeric process exit code.
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Usage => 2,
            Self::Confirmation => 3,
            Self::Unavailable => 69,
            Self::Io => 74,
            Self::Protocol => 76,
            Self::Interrupted => 130,
        }
    }
}

/// Errors surfaced to the CLI user.
///
/// The [`Display`](std::fmt::Display) form is the message: what failed
/// and why, without the `alchemy: ` prefix. Messages from alchemyd pass
/// through unchanged.
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
    #[error("alchemyd closed the connection")]
    Disconnected,
    /// The exchange failed below the message layer.
    #[error("cannot talk to alchemyd: {0}")]
    Ipc(#[from] IpcError),
    /// The daemon answered with an error.
    #[error("alchemyd: {0}")]
    Daemon(String),
    /// The daemon answered with a response this command did not expect.
    #[error("unexpected response from alchemyd: {0}")]
    Unexpected(String),
    /// The daemon predates a request this command needs and answered it
    /// as a bad request.
    #[error("alchemyd {daemon} is too old for `{command}`")]
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
        "cannot use a {scheme}:// daemon from {origin}: remote daemons need authentication, which arrives in M13 (https://github.com/hostingaccessories/alchemy/issues/16)"
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
    /// The command line was invalid.
    #[error("{message}")]
    Usage {
        /// What was wrong with it.
        message: String,
        /// How to get it right, if there is more to say.
        hint: Option<String>,
    },
    /// The command needs confirmation, but there is no TTY to ask on and
    /// `--yes` was not given.
    #[error("`{command}` needs confirmation, but stdin isn't a terminal")]
    ConfirmationRequired {
        /// The command, as typed after `alchemy`.
        command: &'static str,
    },
    /// The user answered no when asked to confirm.
    #[error("`{command}` cancelled: not confirmed")]
    Declined {
        /// The command, as typed after `alchemy`.
        command: &'static str,
    },
    /// The user interrupted the command.
    #[error("interrupted")]
    Interrupted,
    /// The async runtime could not start.
    #[error("cannot start the async runtime: {0}")]
    Runtime(#[source] std::io::Error),
}

impl ClientError {
    /// Convert an unexpected or error response into a [`ClientError`].
    pub fn from_response(resp: Response) -> Self {
        match resp {
            Response::Error { message, .. } => Self::Daemon(message),
            other => Self::Unexpected(format!("{other:?}")),
        }
    }

    /// The exit status for this error.
    pub fn exit(&self) -> Exit {
        match self {
            Self::DaemonUnreachable(_) | Self::Disconnected => Exit::Unavailable,
            Self::ConnectionClosed
            | Self::Ipc(_)
            | Self::Unexpected(_)
            | Self::DaemonTooOld { .. } => Exit::Protocol,
            Self::Output(_) => Exit::Io,
            Self::Daemon(_) | Self::Runtime(_) => Exit::Failure,
            Self::Usage { .. } | Self::RemoteDaemon { .. } | Self::InvalidTarget { .. } => {
                Exit::Usage
            }
            Self::ConfirmationRequired { .. } | Self::Declined { .. } => Exit::Confirmation,
            Self::Interrupted => Exit::Interrupted,
        }
    }

    /// The process exit code for this error.
    pub fn exit_code(&self) -> u8 {
        self.exit().code()
    }

    /// A stable, `snake_case` name for this kind of error: the `kind` in
    /// its JSON form, for scripts to branch on.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DaemonUnreachable(_) => "daemon_unreachable",
            Self::ConnectionClosed => "connection_closed",
            Self::Disconnected => "disconnected",
            Self::Ipc(_) => "protocol",
            Self::Daemon(_) => "daemon",
            Self::Unexpected(_) => "unexpected_response",
            Self::DaemonTooOld { .. } => "daemon_too_old",
            Self::Output(_) => "output",
            Self::Usage { .. } => "usage",
            Self::RemoteDaemon { .. } => "remote_daemon",
            Self::InvalidTarget { .. } => "invalid_target",
            Self::ConfirmationRequired { .. } => "confirmation_required",
            Self::Declined { .. } => "declined",
            Self::Interrupted => "interrupted",
            Self::Runtime(_) => "runtime",
        }
    }

    /// The next step to suggest, if there is one.
    pub fn hint(&self) -> Option<String> {
        let hint = match self {
            Self::DaemonUnreachable(_) => {
                "start alchemyd, or point --socket at the socket it listens on"
            }
            Self::ConnectionClosed => "alchemyd may have crashed; check its logs",
            Self::Disconnected
            | Self::Ipc(IpcError::Io { .. } | IpcError::UnexpectedEof { .. }) => {
                "check that alchemyd is still running with `alchemy status`"
            }
            Self::Ipc(_) | Self::Unexpected(_) => {
                "alchemy and alchemyd may be different versions; upgrade both to the same release"
            }
            Self::DaemonTooOld { .. } => {
                return Some(format!(
                    "upgrade alchemyd to match alchemy {}, then restart it",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            Self::Usage { hint, .. } => return hint.clone(),
            Self::RemoteDaemon { .. } => {
                "forward the remote socket over SSH and pass the local end with --socket"
            }
            Self::ConfirmationRequired { .. } => "pass --yes to go ahead without a prompt",
            Self::Daemon(_)
            | Self::Output(_)
            | Self::InvalidTarget { .. }
            | Self::Declined { .. }
            | Self::Interrupted
            | Self::Runtime(_) => return None,
        };
        Some(hint.to_owned())
    }

    /// Whether reporting this error would only be noise: the user pressed
    /// ctrl-c, or whoever read stdout has gone, as with `| head`. The exit
    /// code still says what happened.
    pub fn is_quiet(&self) -> bool {
        match self {
            Self::Interrupted => true,
            Self::Output(e) => e.kind() == std::io::ErrorKind::BrokenPipe,
            _ => false,
        }
    }

    /// The text report: `alchemy: <message>`, then `hint: <hint>` when
    /// there is one, each ending in a newline.
    pub fn to_text(&self) -> String {
        match self.hint() {
            Some(hint) => format!("alchemy: {self}\nhint: {hint}\n"),
            None => format!("alchemy: {self}\n"),
        }
    }

    /// The `--json` report: one `{"error":{"kind","message","hint"}}`
    /// document on one line, with `hint` null when there is none.
    pub fn to_json(&self) -> String {
        let doc = serde_json::json!({
            "error": {
                "kind": self.kind(),
                "message": self.to_string(),
                "hint": self.hint(),
            }
        });
        format!("{doc}\n")
    }

    /// The usage error for a command line clap rejected.
    ///
    /// clap's report has a first paragraph saying what is wrong, then
    /// optional `tip:` lines and a `Usage:` line. The message is that
    /// first paragraph on one line; the hint is clap's tip, or else a
    /// pointer to the failing command's `--help`.
    pub fn from_clap(err: &clap::Error) -> Self {
        let rendered = err.render().to_string();
        let missing =
            err.kind() == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand;
        usage_from_rendered(&rendered, missing)
    }
}

fn usage_from_rendered(rendered: &str, missing_command: bool) -> ClientError {
    let usage_path = rendered
        .lines()
        .find_map(|line| line.strip_prefix("Usage: "))
        .map_or_else(
            || "alchemy".to_owned(),
            |usage| {
                usage
                    .split_whitespace()
                    .take_while(|word| !word.starts_with(['[', '<']))
                    .collect::<Vec<_>>()
                    .join(" ")
            },
        );
    let message = if missing_command {
        format!("`{usage_path}` needs a command")
    } else {
        rendered
            .lines()
            .take_while(|line| !line.trim().is_empty())
            .map(|line| line.trim().trim_start_matches("error: "))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let tip = rendered
        .lines()
        .find_map(|line| line.trim().strip_prefix("tip: "))
        .map(str::to_owned);
    ClientError::Usage {
        message,
        hint: tip.or_else(|| Some(format!("see `{usage_path} --help`"))),
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
    use clap::Parser;

    use super::*;
    use crate::Cli;

    const CLI: &str = env!("CARGO_PKG_VERSION");

    fn io(kind: std::io::ErrorKind) -> std::io::Error {
        std::io::Error::new(kind, "boom")
    }

    fn json(err: &ClientError) -> serde_json::Value {
        let doc = err.to_json();
        assert_eq!(doc.matches('\n').count(), 1, "{doc}");
        assert!(doc.ends_with('\n'), "{doc}");
        serde_json::from_str(&doc).unwrap()
    }

    #[test]
    fn exit_codes_follow_the_table() {
        let cases = [
            (Exit::Success, 0),
            (Exit::Failure, 1),
            (Exit::Usage, 2),
            (Exit::Confirmation, 3),
            (Exit::Unavailable, 69),
            (Exit::Io, 74),
            (Exit::Protocol, 76),
            (Exit::Interrupted, 130),
        ];
        for (exit, code) in cases {
            assert_eq!(exit.code(), code, "{exit:?}");
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_variant_has_an_exit_code_text_and_json() {
        let too_old_hint = format!("upgrade alchemyd to match alchemy {CLI}, then restart it");
        let running = "check that alchemyd is still running with `alchemy status`";
        let versions =
            "alchemy and alchemyd may be different versions; upgrade both to the same release";
        let decode = serde_json::from_str::<u8>("x").unwrap_err();
        let cases: Vec<(ClientError, u8, &str, String, Option<&str>)> = vec![
            (
                ClientError::DaemonUnreachable("/run/alchemyd.sock: refused".into()),
                69,
                "daemon_unreachable",
                "cannot reach alchemyd: /run/alchemyd.sock: refused".into(),
                Some("start alchemyd, or point --socket at the socket it listens on"),
            ),
            (
                ClientError::ConnectionClosed,
                76,
                "connection_closed",
                "alchemyd closed the connection without responding".into(),
                Some("alchemyd may have crashed; check its logs"),
            ),
            (
                ClientError::Disconnected,
                69,
                "disconnected",
                "alchemyd closed the connection".into(),
                Some(running),
            ),
            (
                ClientError::Ipc(IpcError::Decode(decode)),
                76,
                "protocol",
                "cannot talk to alchemyd: decode: expected value at line 1 column 1".into(),
                Some(versions),
            ),
            (
                ClientError::Ipc(IpcError::Io {
                    kind: std::io::ErrorKind::ConnectionReset,
                }),
                76,
                "protocol",
                "cannot talk to alchemyd: io: connection reset".into(),
                Some(running),
            ),
            (
                ClientError::Daemon("engine monitor stopped: \"x\"".into()),
                1,
                "daemon",
                "alchemyd: engine monitor stopped: \"x\"".into(),
                None,
            ),
            (
                ClientError::Unexpected("Pong".into()),
                76,
                "unexpected_response",
                "unexpected response from alchemyd: Pong".into(),
                Some(versions),
            ),
            (
                ClientError::DaemonTooOld {
                    daemon: "0.0.1".into(),
                    command: "app up",
                },
                76,
                "daemon_too_old",
                "alchemyd 0.0.1 is too old for `app up`".into(),
                Some(&too_old_hint),
            ),
            (
                ClientError::Output(io(std::io::ErrorKind::StorageFull)),
                74,
                "output",
                "cannot write output: boom".into(),
                None,
            ),
            (
                ClientError::Usage {
                    message: "unexpected argument '--nope' found".into(),
                    hint: Some("see `alchemy status --help`".into()),
                },
                2,
                "usage",
                "unexpected argument '--nope' found".into(),
                Some("see `alchemy status --help`"),
            ),
            (
                ClientError::Usage {
                    message: "bad".into(),
                    hint: None,
                },
                2,
                "usage",
                "bad".into(),
                None,
            ),
            (
                ClientError::ConfirmationRequired {
                    command: "app down",
                },
                3,
                "confirmation_required",
                "`app down` needs confirmation, but stdin isn't a terminal".into(),
                Some("pass --yes to go ahead without a prompt"),
            ),
            (
                ClientError::Declined {
                    command: "app down",
                },
                3,
                "declined",
                "`app down` cancelled: not confirmed".into(),
                None,
            ),
            (
                ClientError::Interrupted,
                130,
                "interrupted",
                "interrupted".into(),
                None,
            ),
            (
                ClientError::Runtime(io(std::io::ErrorKind::OutOfMemory)),
                1,
                "runtime",
                "cannot start the async runtime: boom".into(),
                None,
            ),
        ];
        for (err, code, kind, message, hint) in cases {
            assert_eq!(err.exit_code(), code, "{err:?}");
            assert_eq!(err.kind(), kind, "{err:?}");
            assert_eq!(err.to_string(), message, "{err:?}");
            assert_eq!(err.hint().as_deref(), hint, "{err:?}");

            let text = match hint {
                Some(hint) => format!("alchemy: {message}\nhint: {hint}\n"),
                None => format!("alchemy: {message}\n"),
            };
            assert_eq!(err.to_text(), text, "{err:?}");
            assert!(!err.to_text().contains('\x1b'), "{err:?}");

            assert_eq!(
                json(&err),
                serde_json::json!({
                    "error": { "kind": kind, "message": message, "hint": hint }
                }),
                "{err:?}"
            );
        }
    }

    #[test]
    fn interrupts_and_closed_pipes_are_quiet() {
        let cases = [
            (ClientError::Interrupted, true),
            (
                ClientError::Output(io(std::io::ErrorKind::BrokenPipe)),
                true,
            ),
            (
                ClientError::Output(io(std::io::ErrorKind::StorageFull)),
                false,
            ),
            (ClientError::Disconnected, false),
            (ClientError::Declined { command: "x" }, false),
        ];
        for (err, quiet) in cases {
            assert_eq!(err.is_quiet(), quiet, "{err:?}");
        }
    }

    #[test]
    fn from_response_keeps_the_daemon_message() {
        let err = ClientError::from_response(Response::Error {
            code: alchemy_ipc::ErrorCode::Internal,
            message: "boom".into(),
        });
        assert!(
            matches!(&err, ClientError::Daemon(m) if m == "boom"),
            "{err:?}"
        );
        let err = ClientError::from_response(Response::Pong);
        assert!(
            matches!(&err, ClientError::Unexpected(m) if m == "Pong"),
            "{err:?}"
        );
    }

    #[test]
    fn clap_rejections_become_usage_errors() {
        let cases: [(&[&str], &str, &str); 5] = [
            (
                &["alchemy", "status", "--nope"],
                "unexpected argument '--nope' found",
                "see `alchemy status --help`",
            ),
            (
                &["alchemy", "bogus"],
                "unrecognized subcommand 'bogus'",
                "see `alchemy --help`",
            ),
            (
                &["alchemy", "--json", "status", "extra"],
                "unexpected argument 'extra' found",
                "see `alchemy status --help`",
            ),
            (
                &["alchemy", "--socket"],
                "a value is required for '--socket <PATH>' but none was supplied",
                "see `alchemy --help`",
            ),
            (
                &["alchemy"],
                "`alchemy` needs a command",
                "see `alchemy --help`",
            ),
        ];
        for (args, message, hint) in cases {
            let err = Cli::try_parse_from(args).unwrap_err();
            assert!(err.use_stderr(), "{args:?}");
            let err = ClientError::from_clap(&err);
            assert_eq!(err.exit_code(), 2, "{args:?}");
            assert_eq!(err.to_string(), message, "{args:?}");
            assert_eq!(err.hint().as_deref(), Some(hint), "{args:?}");
        }
    }

    #[test]
    fn usage_from_clap_report() {
        let cases = [
            (
                "error: the following required arguments were not provided:\n  <NAME>\n\nUsage: alchemy app up <NAME>\n\nFor more information, try '--help'.\n",
                false,
                "the following required arguments were not provided: <NAME>",
                "see `alchemy app up --help`",
            ),
            (
                "error: unexpected argument '--nope' found\n\n  tip: to pass '--nope' as a value, use '-- --nope'\n\nUsage: alchemy app up <NAME>\n",
                false,
                "unexpected argument '--nope' found",
                "to pass '--nope' as a value, use '-- --nope'",
            ),
            (
                "Manage apps.\n\nUsage: alchemy app <COMMAND>\n\nCommands:\n  up  Start an app\n",
                true,
                "`alchemy app` needs a command",
                "see `alchemy app --help`",
            ),
            ("error: odd\n", false, "odd", "see `alchemy --help`"),
        ];
        for (rendered, missing, message, hint) in cases {
            let err = usage_from_rendered(rendered, missing);
            assert_eq!(err.to_string(), message, "{rendered}");
            assert_eq!(err.hint().as_deref(), Some(hint), "{rendered}");
        }
    }
}
