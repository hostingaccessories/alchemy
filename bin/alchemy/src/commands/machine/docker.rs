//! `alchemy machine docker`: Docker on this machine. For now, starting
//! Docker Desktop.
//!
//! alchemyd decides whether it can start this Docker and runs the launch
//! command itself; this asks it to, then follows the `engine` event until
//! Docker is ready.

use std::io::Write;
use std::time::Duration;

use alchemy_api::ApiEvent;
use alchemy_api::engine::{EngineStart, EngineStartResult};
use alchemy_core::engine::{EngineState, EngineStatus};
use serde::Serialize;

use crate::commands::status::engine_line;
use crate::context::Context;
use crate::daemon::Daemon;
use crate::error::ClientError;

/// `alchemy machine docker start`, as typed after `alchemy`, for errors.
pub(crate) const START_NAME: &str = "machine docker start";

/// The route that starts Docker Desktop.
pub(crate) const START_PATH: &str = "/api/engine/start";

/// How long to wait for Docker Desktop to be ready. A cold start on a
/// slow machine takes a minute or two.
pub(crate) const READY_WAIT: Duration = Duration::from_secs(180);

/// Arguments to `alchemy machine docker`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do with Docker.
    #[command(subcommand)]
    pub command: Verb,
}

/// The `alchemy machine docker` verbs.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Verb {
    /// Start Docker Desktop, and wait until Docker is ready.
    ///
    /// alchemyd opens Docker Desktop: `open -b com.docker.docker` on
    /// macOS, `systemctl --user start docker-desktop` on Linux. Docker
    /// Engine runs as a system service, so starting it needs your
    /// password: this prints the `systemctl` command to run instead.
    Start(StartArgs),
}

/// Arguments to `alchemy machine docker start`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct StartArgs {
    /// Return once Docker Desktop is starting, without waiting until it
    /// is ready.
    #[arg(long)]
    pub no_wait: bool,
}

/// Run `alchemy machine docker`.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    args: Args,
) -> Result<(), ClientError> {
    match args.command {
        Verb::Start(args) => start(ctx, args, READY_WAIT).await,
    }
}

/// Ask alchemyd to start Docker, then, unless `--no-wait`, wait up to
/// `wait` for it to be ready.
pub(crate) async fn start<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    args: StartArgs,
    wait: Duration,
) -> Result<(), ClientError> {
    let answer: EngineStart = ctx.post(START_PATH, &serde_json::json!({})).await?;
    let engine = match answer.result {
        EngineStartResult::AlreadyRunning => None,
        EngineStartResult::Starting if args.no_wait => None,
        EngineStartResult::Starting => {
            if !ctx.json() {
                ctx.out()
                    .progress(format!("{}; waiting until it's ready…", answer.message));
            }
            Some(ready(ctx, wait).await?)
        }
        EngineStartResult::Unsupported => {
            return Err(ClientError::DockerNotStarted {
                message: answer.message,
                command: answer.command,
            });
        }
        _ => {
            return Err(ClientError::Unexpected(format!(
                "alchemyd answered {START_PATH} with a result this alchemy doesn't know"
            )));
        }
    };
    for line in start_lines(&answer, engine.as_ref(), ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// Follow the `engine` event until Docker is ready, for up to `wait`.
async fn ready<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    wait: Duration,
) -> Result<EngineStatus, ClientError> {
    let follow = async {
        let mut events = ctx.events().await?;
        loop {
            match events.next().await? {
                Some(ApiEvent::Engine(engine)) if engine.state == EngineState::Ready => {
                    return Ok(engine);
                }
                Some(_) => {}
                None => return Err(ClientError::Disconnected),
            }
        }
    };
    tokio::time::timeout(wait, follow)
        .await
        .map_err(|_| ClientError::DockerNotStarted {
            message: format!(
                "Docker Desktop didn't become ready within {}; it may still be starting, so check Docker Desktop, then run `alchemy status`",
                minutes(wait)
            ),
            command: None,
        })?
}

fn minutes(wait: Duration) -> String {
    match wait.as_secs() {
        60 => "a minute".to_owned(),
        secs if secs % 60 == 0 => format!("{} minutes", secs / 60),
        secs => format!("{secs} seconds"),
    }
}

/// The `--json` result: alchemyd's answer, and Docker once it is ready
/// when this waited for it.
#[derive(Debug, Serialize)]
struct StartReport<'a> {
    #[serde(flatten)]
    start: &'a EngineStart,
    engine: Option<&'a EngineStatus>,
}

/// What `alchemy machine docker start` prints: Docker's status line once
/// ready, or alchemyd's sentence; or the [`StartReport`].
fn start_lines(answer: &EngineStart, engine: Option<&EngineStatus>, json: bool) -> Vec<String> {
    if json {
        let report = StartReport {
            start: answer,
            engine,
        };
        return vec![serde_json::to_string(&report).unwrap_or_default()];
    }
    vec![engine.map_or_else(|| answer.message.clone(), engine_line)]
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use alchemy_core::engine::EngineFlavor;

    use super::*;
    use crate::daemon::Method;
    use crate::daemon::fake::{FakeDaemon, sse};
    use crate::sse::SseEvent;

    fn answer(result: EngineStartResult, message: &str, command: Option<&str>) -> EngineStart {
        EngineStart {
            result,
            message: message.into(),
            command: command.map(Into::into),
        }
    }

    fn engine(state: EngineState) -> EngineStatus {
        let mut status = EngineStatus::new(state);
        status.flavor = Some(EngineFlavor::DockerDesktop);
        if state == EngineState::Ready {
            status.desktop_version = Some("4.34.2".into());
            status.engine_version = Some("27.3.1".into());
            status.compose_version = Some("2.29.7".into());
        }
        status
    }

    #[allow(clippy::unnecessary_wraps)]
    fn event(state: EngineState) -> Result<SseEvent, ClientError> {
        Ok(sse(
            "engine",
            &serde_json::to_string(&engine(state)).unwrap(),
        ))
    }

    fn post(fake: FakeDaemon, status: u16, answer: &EngineStart) -> FakeDaemon {
        fake.version("1.2.3", 1).reply(
            Method::POST,
            START_PATH,
            status,
            serde_json::to_vec(answer).unwrap(),
        )
    }

    struct Ran {
        result: Result<(), ClientError>,
        out: String,
        progress: String,
        requests: Vec<String>,
    }

    async fn run_on(fake: FakeDaemon, json: bool, no_wait: bool, wait: Duration) -> Ran {
        let mut ctx = Context::buffered(fake, json, START_NAME);
        let result = start(&mut ctx, StartArgs { no_wait }, wait).await;
        Ran {
            result,
            out: ctx.written(),
            progress: ctx.progress_written(),
            requests: ctx.daemon().requests().to_vec(),
        }
    }

    const STARTING: &str = "Docker Desktop is starting";
    const READY: &str = "Docker ready: Desktop 4.34.2, Engine 27.3.1, Compose v2.29.7";

    #[tokio::test]
    async fn waits_for_the_engine_to_be_ready() {
        let fake = post(
            FakeDaemon::new(),
            202,
            &answer(EngineStartResult::Starting, STARTING, None),
        )
        .events([
            event(EngineState::NotRunning),
            Ok(sse("apps", r#"{"apps":[],"unmanaged":[],"dirs":[]}"#)),
            event(EngineState::Starting),
            event(EngineState::Ready),
        ]);
        let ran = run_on(fake, false, false, READY_WAIT).await;
        ran.result.unwrap();
        assert_eq!(ran.out, format!("{READY}\n"));
        assert_eq!(
            ran.progress,
            "Docker Desktop is starting; waiting until it's ready…\n"
        );
        assert_eq!(
            ran.requests,
            [
                "GET /api/version",
                "POST /api/engine/start",
                "GET /api/events"
            ]
        );
    }

    #[tokio::test]
    async fn no_wait_and_already_running_return_at_once() {
        let cases = [
            (
                answer(EngineStartResult::Starting, STARTING, None),
                202,
                true,
            ),
            (
                answer(EngineStartResult::AlreadyRunning, "Docker is running", None),
                200,
                false,
            ),
        ];
        for (answer, status, no_wait) in cases {
            let fake = post(FakeDaemon::new(), status, &answer);
            let ran = run_on(fake, false, no_wait, READY_WAIT).await;
            ran.result.unwrap();
            assert_eq!(ran.out, format!("{}\n", answer.message));
            assert_eq!(ran.progress, "");
            assert_eq!(ran.requests.len(), 2, "{:?}", ran.requests);
        }
    }

    #[tokio::test]
    async fn json_is_the_answer_and_the_ready_engine() {
        let starting = answer(EngineStartResult::Starting, STARTING, None);
        let fake = post(FakeDaemon::new(), 202, &starting).events([event(EngineState::Ready)]);
        let ran = run_on(fake, true, false, READY_WAIT).await;
        ran.result.unwrap();
        let got: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(got["result"], "starting");
        assert_eq!(got["message"], STARTING);
        assert_eq!(got["command"], serde_json::Value::Null);
        assert_eq!(got["engine"]["state"], "ready");
        assert_eq!(ran.progress, "");

        let fake = post(FakeDaemon::new(), 202, &starting);
        let ran = run_on(fake, true, true, READY_WAIT).await;
        let got: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(got["engine"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn docker_engine_gets_the_systemctl_command() {
        let refused = answer(
            EngineStartResult::Unsupported,
            "Docker Engine runs as a system service, so starting it needs your password; run the command in a terminal",
            Some("sudo systemctl start docker"),
        );
        let ran = run_on(
            post(FakeDaemon::new(), 200, &refused),
            false,
            false,
            READY_WAIT,
        )
        .await;
        let err = ran.result.unwrap_err();
        assert_eq!(err.kind(), "docker_not_started");
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.to_string(), refused.message);
        assert_eq!(
            err.hint().as_deref(),
            Some("run `sudo systemctl start docker` in a terminal")
        );
        assert_eq!(ran.out, "");
    }

    #[tokio::test]
    async fn failures_are_errors() {
        let starting = answer(EngineStartResult::Starting, STARTING, None);
        let missing = FakeDaemon::new().version("1.2.3", 1).error(
            Method::POST,
            START_PATH,
            422,
            "unsupported",
            "Docker Desktop isn't installed",
        );
        let ended = post(FakeDaemon::new(), 202, &starting).events([event(EngineState::Starting)]);
        let too_old = FakeDaemon::new().version("0.0.1", 1).error(
            Method::POST,
            START_PATH,
            404,
            "not_found",
            "no",
        );
        let cases = [
            (
                missing,
                "daemon",
                "alchemyd: Docker Desktop isn't installed",
            ),
            (ended, "disconnected", "alchemyd closed the connection"),
            (
                too_old,
                "daemon_too_old",
                "alchemyd 0.0.1 is too old for `machine docker start`",
            ),
        ];
        for (fake, kind, message) in cases {
            let err = run_on(fake, false, false, READY_WAIT)
                .await
                .result
                .unwrap_err();
            assert_eq!((err.kind(), err.to_string().as_str()), (kind, message));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_waiting_with_a_clear_message() {
        let starting = answer(EngineStartResult::Starting, STARTING, None);
        let fake = post(FakeDaemon::new(), 202, &starting).events_then_hang([
            event(EngineState::Starting),
            Ok(sse("apps", r#"{"apps":[],"unmanaged":[],"dirs":[]}"#)),
        ]);
        let mut ctx = Context::buffered(fake, false, START_NAME);
        let pending = start(&mut ctx, StartArgs::default(), Duration::from_secs(120));
        let err = pending.await.unwrap_err();
        assert_eq!(err.kind(), "docker_not_started");
        assert_eq!(
            err.to_string(),
            "Docker Desktop didn't become ready within 2 minutes; it may still be starting, so check Docker Desktop, then run `alchemy status`"
        );
        assert_eq!(err.hint(), None);
    }

    #[test]
    fn minutes_reads_naturally() {
        let cases = [(60, "a minute"), (180, "3 minutes"), (90, "90 seconds")];
        for (secs, want) in cases {
            assert_eq!(minutes(Duration::from_secs(secs)), want);
        }
    }
}
