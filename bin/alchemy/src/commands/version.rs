//! `alchemy version`: the versions of alchemy and alchemyd, and the IPC
//! protocol each speaks.
//!
//! `alchemy --version` prints the CLI's version without contacting the
//! daemon; this asks the daemon too, to show when the two differ.

use std::io::Write;
use std::path::Path;

use alchemy_ipc::PROTOCOL_VERSION;

use super::status::daemon_line;
use crate::context::Context;
use crate::daemon::{Daemon, DaemonInfo};
use crate::error::ClientError;

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "version";

/// Arguments to `alchemy version`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {}

/// What this alchemy says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CliInfo {
    /// The CLI's release version.
    pub(crate) version: &'static str,
    /// The IPC protocol version the CLI speaks.
    pub(crate) protocol: u32,
}

impl CliInfo {
    /// This build of alchemy.
    pub(crate) const CURRENT: Self = Self {
        version: env!("CARGO_PKG_VERSION"),
        protocol: PROTOCOL_VERSION,
    };
}

/// Run `alchemy version`.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    _args: Args,
) -> Result<(), ClientError> {
    let daemon = ctx.daemon_info().await?.clone();
    for line in version_lines(&CliInfo::CURRENT, &daemon, ctx.socket(), ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// What `alchemy version` prints: a line each for alchemy and alchemyd,
/// or one JSON object whose daemon fields match `status --json`.
pub(crate) fn version_lines(
    cli: &CliInfo,
    daemon: &DaemonInfo,
    socket: &Path,
    json: bool,
) -> Vec<String> {
    if json {
        let doc = serde_json::json!({
            "cli": cli.version,
            "cli_protocol": cli.protocol,
            "daemon": daemon.version,
            "protocol": daemon.protocol,
            "socket": socket,
        });
        vec![doc.to_string()]
    } else {
        vec![
            format!("alchemy {} (protocol {})", cli.version, cli.protocol),
            daemon_line(daemon, socket),
        ]
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
    use alchemy_ipc::Request;

    use super::*;
    use crate::daemon::fake::{FAKE_SOCKET, FakeDaemon};

    const CLI: CliInfo = CliInfo {
        version: "1.2.3",
        protocol: 1,
    };

    fn daemon(version: &str, protocol: u32) -> DaemonInfo {
        DaemonInfo {
            version: version.into(),
            protocol,
        }
    }

    #[test]
    fn lines_per_format() {
        let socket = Path::new("/run/alchemyd.sock");
        let cases = [
            (
                daemon("1.2.3", 1),
                "alchemyd 1.2.3 (protocol 1) at /run/alchemyd.sock",
            ),
            (
                daemon("0.9.0", 2),
                "alchemyd 0.9.0 (protocol 2) at /run/alchemyd.sock",
            ),
        ];
        for (daemon, line) in cases {
            assert_eq!(
                version_lines(&CLI, &daemon, socket, false),
                ["alchemy 1.2.3 (protocol 1)", line],
                "{daemon:?}"
            );
            let json = version_lines(&CLI, &daemon, socket, true);
            assert_eq!(json.len(), 1);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&json[0]).unwrap(),
                serde_json::json!({
                    "cli": "1.2.3",
                    "cli_protocol": 1,
                    "daemon": daemon.version,
                    "protocol": daemon.protocol,
                    "socket": "/run/alchemyd.sock",
                }),
                "{daemon:?}"
            );
        }
    }

    #[test]
    fn current_is_this_build() {
        assert_eq!(CliInfo::CURRENT.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(CliInfo::CURRENT.protocol, PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn asks_the_daemon_only_for_its_version() {
        for json in [false, true] {
            let fake = FakeDaemon::new().version("0.9.0", 1);
            let mut ctx = Context::buffered(fake, json, NAME);
            run(&mut ctx, Args::default()).await.unwrap();
            let want = version_lines(
                &CliInfo::CURRENT,
                &daemon("0.9.0", 1),
                Path::new(FAKE_SOCKET),
                json,
            );
            assert_eq!(ctx.written(), format!("{}\n", want.join("\n")), "{json}");
            assert_eq!(ctx.daemon().requests(), [Request::Version], "{json}");
        }
    }

    #[tokio::test]
    async fn an_unreachable_daemon_prints_nothing() {
        let fake = FakeDaemon::new().fail(
            Request::Version,
            ClientError::DaemonUnreachable(format!("{FAKE_SOCKET}: refused")),
        );
        let mut ctx = Context::buffered(fake, false, NAME);
        let err = run(&mut ctx, Args::default()).await.unwrap_err();
        assert!(matches!(err, ClientError::DaemonUnreachable(_)), "{err:?}");
        assert_eq!(err.exit_code(), 69);
        assert_eq!(ctx.written(), "");
    }
}
