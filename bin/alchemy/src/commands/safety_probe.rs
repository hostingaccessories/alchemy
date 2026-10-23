//! `alchemy safety-probe`: a stand-in destructive command, so the e2e
//! tests can drive the safety flags before a real one exists.
//!
//! Built only with the `test-command` feature and hidden from help. Its
//! "change" is a harmless `Ping`. A real command asks alchemyd for its
//! plan; alchemyd has no plan request yet, so the probe's one-step plan
//! is built from the daemon's handshake instead.

use std::io::Write;
use std::path::Path;

use alchemy_ipc::{Request, Response};

use crate::context::Context;
use crate::daemon::{Daemon, DaemonInfo};
use crate::error::ClientError;
use crate::prompt::Prompt;
use crate::safety::{self, Mode, Safety};

/// The command's name, for errors.
pub(crate) const NAME: &str = "safety-probe";

/// What the probe asks before it pings.
pub const QUESTION: &str = "Ping alchemyd?";

/// Arguments to `alchemy safety-probe`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// The shared safety flags.
    #[command(flatten)]
    pub safety: Safety,
}

/// Run `alchemy safety-probe`: confirm, then ping alchemyd, or with
/// `--dry-run` print the plan without pinging.
pub(crate) async fn run<D: Daemon, W: Write, P: Prompt>(
    ctx: &mut Context<D, W>,
    prompt: &mut P,
    args: Args,
) -> Result<(), ClientError> {
    match safety::confirm(prompt, &args.safety, NAME, QUESTION).await? {
        Mode::DryRun => {
            let daemon = ctx.daemon_info().await?.clone();
            let line = plan_line(&step(&daemon, ctx.socket()), ctx.json());
            ctx.out().line(line)
        }
        Mode::Execute => {
            let daemon = ctx.daemon_info().await?.clone();
            match ctx.request(&Request::Ping).await? {
                Response::Pong => {
                    let line = done_line(&step(&daemon, ctx.socket()), ctx.json());
                    ctx.out().line(line)
                }
                other => Err(ClientError::from_response(other)),
            }
        }
    }
}

fn step(daemon: &DaemonInfo, socket: &Path) -> String {
    format!("ping alchemyd {} at {}", daemon.version, socket.display())
}

/// What `--dry-run` prints: the plan, changing nothing.
pub(crate) fn plan_line(step: &str, json: bool) -> String {
    if json {
        serde_json::json!({ "dry_run": true, "plan": [step] }).to_string()
    } else {
        format!("would {step}")
    }
}

/// What the probe prints once it has pinged.
pub(crate) fn done_line(step: &str, json: bool) -> String {
    if json {
        serde_json::json!({ "dry_run": false, "done": [step] }).to_string()
    } else {
        format!("did {step}")
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
    use crate::daemon::fake::FakeDaemon;
    use crate::prompt::FakePrompt;

    const STEP: &str = "ping alchemyd 1.2.3 at /run/alchemyd.sock";

    fn safety(dry_run: bool, yes: bool) -> Args {
        Args {
            safety: Safety {
                dry_run,
                force: false,
                yes,
            },
        }
    }

    #[test]
    fn lines_per_mode_and_format() {
        assert_eq!(plan_line(STEP, false), format!("would {STEP}"));
        assert_eq!(done_line(STEP, false), format!("did {STEP}"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&plan_line(STEP, true)).unwrap(),
            serde_json::json!({ "dry_run": true, "plan": [STEP] })
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&done_line(STEP, true)).unwrap(),
            serde_json::json!({ "dry_run": false, "done": [STEP] })
        );
    }

    #[tokio::test]
    async fn only_a_confirmed_run_changes_anything() {
        let version = || FakeDaemon::new().version("1.2.3", 1);
        let pinged = || version().reply(Request::Ping, Response::Pong);
        let cases = [
            (
                pinged(),
                FakePrompt::tty([true]),
                safety(false, false),
                Ok(format!("did {STEP}\n")),
                vec![Request::Version, Request::Ping],
            ),
            (
                FakeDaemon::new(),
                FakePrompt::tty([false]),
                safety(false, false),
                Err(3),
                vec![],
            ),
            (
                FakeDaemon::new(),
                FakePrompt::no_tty(),
                safety(false, false),
                Err(3),
                vec![],
            ),
            (
                pinged(),
                FakePrompt::no_tty(),
                safety(false, true),
                Ok(format!("did {STEP}\n")),
                vec![Request::Version, Request::Ping],
            ),
            (
                version(),
                FakePrompt::no_tty(),
                safety(true, false),
                Ok(format!("would {STEP}\n")),
                vec![Request::Version],
            ),
        ];
        for (fake, mut prompt, args, want, requests) in cases {
            let mut ctx = Context::buffered(fake, false, NAME);
            let got = run(&mut ctx, &mut prompt, args.clone())
                .await
                .map(|()| ctx.written())
                .map_err(|e| e.exit_code());
            assert_eq!(got, want, "{args:?}");
            assert_eq!(ctx.daemon().requests(), requests, "{args:?}");
        }
    }
}
