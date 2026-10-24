//! End-to-end: a real alchemyd on a tempdir socket, with a fake engine,
//! driven through the CLI's public API.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use alchemy::commands::{safety_probe, status};
use alchemy::prompt::FakePrompt;
use alchemy::safety::Safety;
use alchemy::{Cli, ClientError, Color, Command, DAEMON_ENV};
use alchemy_api::{
    API_VERSION, ErrorCode, ErrorResponse, HealthResponse, HealthStatus, VersionResponse,
};
use alchemy_core::engine::{EngineFlavor, EngineState, EngineStatus};
use alchemyd::engine::EngineSource;
use alchemyd::{Config, Deps, State};
use clap::{CommandFactory, Parser};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const DEADLINE: Duration = Duration::from_secs(10);

struct FixedEngine(EngineStatus);

impl EngineSource for FixedEngine {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        state.set_engine(self.0);
        let _ = shutdown.wait_for(|stop| *stop).await;
    }
}

struct Daemon {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    state: State,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), String>>,
}

impl Daemon {
    async fn start(engine: EngineStatus) -> Self {
        Self::start_with(engine, |deps| deps).await
    }

    /// [`Self::start`], with `deps` changed by `change` first.
    async fn start_with(
        engine: EngineStatus,
        change: impl FnOnce(Deps<FixedEngine>) -> Deps<FixedEngine>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("alchemyd.sock");
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            socket: socket.clone(),
            state_dir: dir.path().join("state"),
        };
        let deps = change(Deps::new(FixedEngine(engine.clone())));
        let state = deps.state.clone();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            alchemyd::run_with(config, deps, async {
                let _ = stop_rx.await;
            })
            .await
            .map_err(|e| format!("{e:#}"))
        });
        let mut engines = state.watch_engine();
        tokio::time::timeout(DEADLINE, engines.wait_for(|s| *s == engine))
            .await
            .unwrap()
            .unwrap();
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Self {
            _dir: dir,
            socket,
            state,
            stop,
            task,
        }
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(DEADLINE, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

fn cli(socket: &Path, json: bool, watch: bool) -> Cli {
    Cli {
        json,
        daemon: None,
        socket: Some(socket.to_owned()),
        command: Command::Status(status::Args { watch }),
    }
}

async fn run(cli: Cli) -> Result<String, ClientError> {
    let mut out = Vec::new();
    alchemy::run(cli, &mut out, Color::Off).await?;
    Ok(String::from_utf8(out).unwrap())
}

/// Forwards each complete output line to a channel, so a test can follow
/// `--watch` while it runs.
struct LineSink {
    buf: Vec<u8>,
    lines: mpsc::UnboundedSender<String>,
}

impl Write for LineSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        while let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=end).collect();
            let line = String::from_utf8(line).unwrap();
            let _ = self.lines.send(line.trim_end().to_owned());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Watch {
    lines: mpsc::UnboundedReceiver<String>,
    task: JoinHandle<Result<(), ClientError>>,
}

impl Watch {
    fn start(cli: Cli) -> Self {
        let (tx, lines) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut sink = LineSink {
                buf: Vec::new(),
                lines: tx,
            };
            alchemy::run(cli, &mut sink, Color::Off).await
        });
        Self { lines, task }
    }

    async fn line(&mut self) -> String {
        tokio::time::timeout(DEADLINE, self.lines.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn finish(self) -> Result<(), ClientError> {
        tokio::time::timeout(DEADLINE, self.task)
            .await
            .unwrap()
            .unwrap()
    }
}

fn ready_desktop() -> EngineStatus {
    let mut status = EngineStatus::new(EngineState::Ready);
    status.flavor = Some(EngineFlavor::DockerDesktop);
    status.desktop_version = Some("4.34.2".into());
    status.engine_version = Some("27.3.1".into());
    status.api_version = Some("1.47".into());
    status.compose_version = Some("2.29.7".into());
    status.endpoint = Some("unix:///var/run/docker.sock".into());
    status
}

fn daemon_line(socket: &Path) -> String {
    format!(
        "alchemyd {} (protocol {API_VERSION}) at {}",
        env!("CARGO_PKG_VERSION"),
        socket.display()
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_the_daemon_and_docker() {
    let daemon = Daemon::start(ready_desktop()).await;
    let out = run(cli(&daemon.socket, false, false)).await.unwrap();
    assert_eq!(
        out,
        format!(
            "{}\nDocker ready: Desktop 4.34.2, Engine 27.3.1, Compose v2.29.7\n",
            daemon_line(&daemon.socket)
        )
    );

    daemon
        .state
        .set_engine(EngineStatus::new(EngineState::NotInstalled));
    let out = run(cli(&daemon.socket, false, false)).await.unwrap();
    assert_eq!(
        out.lines().nth(1),
        Some("Docker isn't installed: install Docker Desktop or Docker Engine")
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_json_adds_the_engine() {
    let daemon = Daemon::start(ready_desktop()).await;
    let out = run(cli(&daemon.socket, true, false)).await.unwrap();
    assert_eq!(out.lines().count(), 1, "{out}");
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["daemon"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["protocol"], API_VERSION);
    assert_eq!(json["socket"], daemon.socket.to_str().unwrap());
    assert_eq!(
        json["engine"],
        serde_json::to_value(ready_desktop()).unwrap()
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_watch_prints_a_line_per_change_until_the_daemon_stops() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::NotRunning)).await;
    let mut watch = Watch::start(cli(&daemon.socket, false, true));
    assert_eq!(watch.line().await, daemon_line(&daemon.socket));
    assert_eq!(watch.line().await, "Docker isn't running");

    daemon
        .state
        .set_engine(EngineStatus::new(EngineState::Starting));
    assert_eq!(watch.line().await, "Docker is starting…");

    let mut api_only = EngineStatus::new(EngineState::Starting);
    api_only.api_version = Some("1.47".into());
    daemon.state.set_engine(api_only);
    daemon.state.set_engine(ready_desktop());
    assert_eq!(
        watch.line().await,
        "Docker ready: Desktop 4.34.2, Engine 27.3.1, Compose v2.29.7"
    );

    let mut unsupported = EngineStatus::new(EngineState::Unsupported);
    unsupported.endpoint = Some("tcp://10.0.0.5:2375".into());
    daemon.state.set_engine(unsupported);
    assert_eq!(
        watch.line().await,
        "Docker endpoint tcp://10.0.0.5:2375 isn't supported: Alchemy needs a unix:// socket"
    );

    daemon.stop().await;
    let err = watch.finish().await.unwrap_err();
    assert!(matches!(err, ClientError::Disconnected), "{err:?}");
    assert_eq!(err.exit_code(), 69);
}

#[tokio::test(flavor = "multi_thread")]
async fn status_watch_json_prints_one_object_per_change() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::Starting)).await;
    let mut watch = Watch::start(cli(&daemon.socket, true, true));
    let first: serde_json::Value = serde_json::from_str(&watch.line().await).unwrap();
    assert_eq!(first["daemon"], env!("CARGO_PKG_VERSION"));
    assert_eq!(first["engine"]["state"], "starting");

    daemon.state.set_engine(ready_desktop());
    let next: serde_json::Value = serde_json::from_str(&watch.line().await).unwrap();
    assert_eq!(next["socket"], daemon.socket.to_str().unwrap());
    assert_eq!(
        next["engine"],
        serde_json::to_value(ready_desktop()).unwrap()
    );

    watch.task.abort();
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn machine_and_app_stats_show_the_daemons_latest_sample() {
    use std::sync::Arc;

    use alchemyd::apps::{AppsDeps, SystemFs};
    use alchemyd::compose::FakeComposeConfig;

    let apps = tempfile::tempdir().unwrap();
    let home = apps.path().canonicalize().unwrap();
    std::fs::create_dir(home.join("blog")).unwrap();
    std::fs::write(home.join("blog/compose.yaml"), "name: my-blog\n").unwrap();
    let config = FakeComposeConfig::new();
    config.project(home.join("blog"), "my-blog", &[]);
    let daemon = Daemon::start_with(EngineStatus::new(EngineState::Ready), |deps| {
        deps.with_apps(
            AppsDeps::new(Arc::new(SystemFs), Arc::new(config), Some(home))
                .with_scan_interval(Duration::from_millis(20)),
        )
    })
    .await;
    let mut listed = daemon.state.watch_apps();
    tokio::time::timeout(
        DEADLINE,
        listed.wait_for(|list| list.apps.first().is_some_and(|app| app.project.is_some())),
    )
    .await
    .unwrap()
    .unwrap();
    let mut metrics = daemon.state.watch_metrics();
    tokio::time::timeout(DEADLINE, metrics.wait_for(|m| m.at.is_some()))
        .await
        .unwrap()
        .unwrap();
    let socket = daemon.socket.to_str().unwrap();
    let parse = |args: &[&str]| {
        let mut argv = vec!["alchemy", "--socket", socket];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).unwrap()
    };

    let out = run(parse(&["machine", "stats"])).await.unwrap();
    assert!(out.starts_with("CPU "), "{out}");
    assert!(out.contains("\nMemory "), "{out}");

    let out = run(parse(&["machine", "stats", "--json"])).await.unwrap();
    assert_eq!(out.lines().count(), 1, "{out}");
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["interval"], 60);
    assert!(
        json["machine"]["memory_total"].as_u64().unwrap() > 0,
        "{out}"
    );

    let out = run(parse(&["app", "stats", "blog"])).await.unwrap();
    assert_eq!(out, "blog has no running containers\n");
    let out = run(parse(&["app", "stats", "blog", "--json"]))
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["machine"], serde_json::Value::Null);
    assert_eq!(json["projects"], serde_json::json!([]));

    let err = run(parse(&["app", "stats", "my-blog"])).await.unwrap_err();
    assert_eq!(err.to_string(), "alchemyd: no app is named \"my-blog\"");

    daemon.stop().await;
}

#[tokio::test]
async fn version_reports_alchemy_and_the_daemon() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::Starting)).await;
    let socket = daemon.socket.to_str().unwrap();
    let version = env!("CARGO_PKG_VERSION");

    let cli = Cli::try_parse_from(["alchemy", "--socket", socket, "version"]).unwrap();
    assert_eq!(
        run(cli).await.unwrap(),
        format!(
            "alchemy {version} (protocol {API_VERSION})\n{}\n",
            daemon_line(&daemon.socket)
        )
    );

    let cli = Cli::try_parse_from(["alchemy", "--socket", socket, "version", "--json"]).unwrap();
    let out = run(cli).await.unwrap();
    assert_eq!(out.lines().count(), 1, "{out}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap(),
        serde_json::json!({
            "cli": version,
            "cli_protocol": API_VERSION,
            "daemon": version,
            "protocol": API_VERSION,
            "socket": socket,
        })
    );
    daemon.stop().await;
}

/// Answers like an alchemyd from before the engine routes existed: it
/// knows `/api/health` and `/api/version`, and answers anything else with
/// a `not_found` error.
fn old_daemon(socket: &Path) -> JoinHandle<()> {
    let app = axum::Router::new()
        .route(
            "/api/health",
            axum::routing::get(|| async {
                axum::Json(HealthResponse {
                    status: HealthStatus::Ok,
                })
            }),
        )
        .route(
            "/api/version",
            axum::routing::get(|| async {
                axum::Json(VersionResponse {
                    version: "0.0.1".into(),
                    api: 1,
                })
            }),
        )
        .fallback(|| async {
            (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse::new(ErrorCode::NotFound, "not found")),
            )
        });
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_daemon_is_reported_as_too_old() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("old.sock");
    let server = old_daemon(&socket);
    for watch in [false, true] {
        let err = run(cli(&socket, false, watch)).await.unwrap_err();
        assert!(
            matches!(&err, ClientError::DaemonTooOld { daemon, command: "status" } if daemon == "0.0.1"),
            "{err:?}"
        );
        assert!(err.to_string().contains("too old"), "{err}");
        assert_eq!(err.exit_code(), 76);
    }
    server.abort();
}

fn probe(socket: &Path, json: bool, safety: Safety) -> Cli {
    Cli {
        json,
        daemon: None,
        socket: Some(socket.to_owned()),
        command: Command::SafetyProbe(safety_probe::Args { safety }),
    }
}

async fn run_prompted(cli: Cli, prompt: &mut FakePrompt) -> Result<String, ClientError> {
    let mut out = Vec::new();
    alchemy::run_with(cli, &mut out, Color::Off, prompt).await?;
    Ok(String::from_utf8(out).unwrap())
}

fn yes() -> Safety {
    Safety {
        yes: true,
        ..Safety::default()
    }
}

fn dry_run() -> Safety {
    Safety {
        dry_run: true,
        ..Safety::default()
    }
}

fn probe_step(socket: &Path) -> String {
    format!(
        "ping alchemyd {} at {}",
        env!("CARGO_PKG_VERSION"),
        socket.display()
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_destructive_command_asks_on_a_terminal() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::Ready)).await;
    let step = probe_step(&daemon.socket);

    let mut prompt = FakePrompt::tty([true]);
    let out = run_prompted(probe(&daemon.socket, false, Safety::default()), &mut prompt)
        .await
        .unwrap();
    assert_eq!(out, format!("did {step}\n"));
    assert_eq!(prompt.asked(), [safety_probe::QUESTION]);

    let mut prompt = FakePrompt::tty([false]);
    let err = run_prompted(probe(&daemon.socket, false, Safety::default()), &mut prompt)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ClientError::Declined {
                command: "safety-probe"
            }
        ),
        "{err:?}"
    );
    assert_eq!(err.exit_code(), 3);
    assert_eq!(prompt.asked(), [safety_probe::QUESTION]);

    let force = Safety {
        force: true,
        ..Safety::default()
    };
    let mut prompt = FakePrompt::tty([false]);
    let err = run_prompted(probe(&daemon.socket, false, force), &mut prompt)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 3, "--force must not skip the question");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn yes_skips_the_question_without_a_terminal() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::Ready)).await;
    let mut prompt = FakePrompt::no_tty();
    let out = run_prompted(probe(&daemon.socket, false, yes()), &mut prompt)
        .await
        .unwrap();
    assert_eq!(out, format!("did {}\n", probe_step(&daemon.socket)));
    assert_eq!(prompt.asked(), [] as [&str; 0]);

    let mut prompt = FakePrompt::tty([false]);
    let out = run_prompted(probe(&daemon.socket, true, yes()), &mut prompt)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["done"][0], probe_step(&daemon.socket));
    assert_eq!(prompt.asked(), [] as [&str; 0]);
    daemon.stop().await;
}

#[tokio::test]
async fn without_a_terminal_or_yes_it_refuses_before_reaching_the_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("nope.sock");
    for json in [false, true] {
        let mut prompt = FakePrompt::no_tty();
        let err = run_prompted(probe(&socket, json, Safety::default()), &mut prompt)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                ClientError::ConfirmationRequired {
                    command: "safety-probe"
                }
            ),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), 3);
        assert!(err.to_text().contains("hint: pass --yes"), "{err}");
        assert_eq!(prompt.asked(), [] as [&str; 0]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_prints_the_plan_and_asks_nothing() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::Ready)).await;
    let step = probe_step(&daemon.socket);
    for prompt in [FakePrompt::no_tty(), FakePrompt::tty([true])] {
        let mut prompt = prompt;
        let out = run_prompted(probe(&daemon.socket, false, dry_run()), &mut prompt)
            .await
            .unwrap();
        assert_eq!(out, format!("would {step}\n"));
        assert_eq!(prompt.asked(), [] as [&str; 0]);
    }

    let mut prompt = FakePrompt::no_tty();
    let out = run_prompted(probe(&daemon.socket, true, dry_run()), &mut prompt)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap(),
        serde_json::json!({ "dry_run": true, "plan": [step] })
    );
    daemon.stop().await;
}

#[test]
fn safety_flags_belong_to_destructive_commands_only() {
    let cli =
        Cli::try_parse_from(["alchemy", "safety-probe", "--dry-run", "--force", "-y"]).unwrap();
    let Command::SafetyProbe(args) = cli.command else {
        panic!("{:?}", cli.command);
    };
    assert_eq!(
        args.safety,
        Safety {
            dry_run: true,
            force: true,
            yes: true
        }
    );

    for flag in ["--dry-run", "--force", "--yes"] {
        let err = Cli::try_parse_from(["alchemy", "status", flag]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::UnknownArgument,
            "{flag}"
        );
    }

    let help = Cli::command().render_long_help().to_string();
    assert!(!help.contains("safety-probe"), "{help}");
}

#[tokio::test]
async fn missing_daemon_is_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    for watch in [false, true] {
        let err = run(cli(&tmp.path().join("nope.sock"), false, watch))
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::DaemonUnreachable(_)), "{err:?}");
        assert_eq!(err.exit_code(), 69);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_unix_daemon_url_reaches_the_same_socket_as_socket() {
    let daemon = Daemon::start(ready_desktop()).await;
    let url = format!("unix://{}", daemon.socket.display());
    let cli = Cli::try_parse_from(["alchemy", "--daemon", &url, "status", "--json"]).unwrap();
    let json: serde_json::Value = serde_json::from_str(&run(cli).await.unwrap()).unwrap();
    assert_eq!(json["socket"], daemon.socket.to_str().unwrap());
    daemon.stop().await;
}

#[test]
fn daemon_and_socket_flags_together_are_a_usage_error() {
    let err = Cli::try_parse_from([
        "alchemy",
        "--daemon",
        "unix:///a.sock",
        "--socket",
        "/b.sock",
        "status",
    ])
    .unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    assert_eq!(err.exit_code(), 2);
}

/// Runs the real `alchemy` binary with `args` and `env`, and without the
/// caller's own daemon variables.
async fn alchemy_bin(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_alchemy"));
    cmd.args(args)
        .env_remove(DAEMON_ENV)
        .env_remove(alchemy_api::SOCKET_ENV)
        .envs(env.iter().copied());
    tokio::task::spawn_blocking(move || cmd.output())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_remote_daemon_is_refused_until_auth_lands() {
    let flag = alchemy_bin(&["--daemon", "https://x", "status"], &[]).await;
    let env = alchemy_bin(&["status"], &[(DAEMON_ENV, "https://x")]).await;
    for (out, origin) in [(flag, "--daemon"), (env, DAEMON_ENV)] {
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.status.code(), Some(2), "{stderr}");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "");
        assert!(
            stderr.starts_with(&format!(
                "alchemy: cannot use a https:// daemon from {origin}: remote daemons need authentication, which arrives in M13"
            )),
            "{stderr}"
        );
        assert!(stderr.contains("issues/16"), "{stderr}");
        assert!(stderr.contains("\nhint: "), "{stderr}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_socket_flag_beats_the_daemon_env() {
    let daemon = Daemon::start(ready_desktop()).await;
    let socket = daemon.socket.to_str().unwrap().to_owned();
    let out = alchemy_bin(
        &["--socket", &socket, "status"],
        &[(DAEMON_ENV, "https://x")],
    )
    .await;
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "{stdout}");
    assert_eq!(stdout.lines().next().unwrap(), daemon_line(&daemon.socket));
    daemon.stop().await;
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn results_go_to_stdout_and_nothing_to_stderr() {
    let daemon = Daemon::start(ready_desktop()).await;
    let socket = daemon.socket.to_str().unwrap().to_owned();
    for args in [vec!["--socket", &socket, "status"], vec!["--help"]] {
        let out = alchemy_bin(&args, &[("NO_COLOR", "1")]).await;
        let stdout = text(out.stdout);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert_eq!(text(out.stderr), "", "{args:?}");
        assert_ne!(stdout, "", "{args:?}");
        assert!(!stdout.contains('\x1b'), "{stdout}");
    }
    daemon.stop().await;
}

#[tokio::test]
async fn failures_are_reported_on_stderr_as_text_or_json() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope.sock");
    let missing = missing.to_str().unwrap();
    let cases: [(&[&str], i32, &str, &str); 2] = [
        (
            &["--socket", missing, "status"],
            69,
            "daemon_unreachable",
            "start alchemyd, or point --socket at the socket it listens on",
        ),
        (
            &["status", "--nope"],
            2,
            "usage",
            "see `alchemy status --help`",
        ),
    ];
    for (args, code, kind, hint) in cases {
        let out = alchemy_bin(args, &[]).await;
        assert_eq!(out.status.code(), Some(code), "{args:?}");
        assert_eq!(text(out.stdout), "", "{args:?}");
        let stderr = text(out.stderr);
        let lines: Vec<_> = stderr.lines().collect();
        assert_eq!(lines.len(), 2, "{stderr}");
        assert!(lines[0].starts_with("alchemy: "), "{stderr}");
        assert_eq!(lines[1], format!("hint: {hint}"), "{stderr}");

        let json_args: Vec<_> = ["--json"].iter().chain(args).copied().collect();
        let out = alchemy_bin(&json_args, &[]).await;
        assert_eq!(out.status.code(), Some(code), "{args:?}");
        assert_eq!(text(out.stdout), "", "{args:?}");
        let stderr = text(out.stderr);
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        let json: serde_json::Value = serde_json::from_str(&stderr).unwrap();
        assert_eq!(json["error"]["kind"], kind, "{stderr}");
        assert_eq!(json["error"]["hint"], hint, "{stderr}");
        assert_eq!(
            format!("alchemy: {}", json["error"]["message"].as_str().unwrap()),
            lines[0],
            "{stderr}"
        );
    }
}

/// Run `alchemy` with `args` against `socket`, as the binary parses them.
async fn run_args(socket: &Path, args: &[&str]) -> Result<String, ClientError> {
    let mut command_line = vec!["alchemy", "--socket", socket.to_str().unwrap()];
    command_line.extend_from_slice(args);
    run(Cli::try_parse_from(command_line).unwrap()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn apps_are_found_in_the_folder_the_settings_name() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::Ready)).await;
    let apps = tempfile::tempdir().unwrap();
    let home = apps.path().canonicalize().unwrap();
    std::fs::create_dir(home.join("blog")).unwrap();
    std::fs::write(home.join("blog/compose.yaml"), "services: {}\n").unwrap();
    let home_str = home.to_str().unwrap();

    let out = run_args(&daemon.socket, &["settings", "show"])
        .await
        .unwrap();
    assert_eq!(out, "App home:           none\nOther app folders:  none\n");
    let out = run_args(&daemon.socket, &["settings", "set", "app-home", home_str])
        .await
        .unwrap();
    assert_eq!(
        out,
        format!("App home:           {home_str}\nOther app folders:  none\n")
    );

    let deadline = tokio::time::Instant::now() + DEADLINE;
    let list = loop {
        let out = run_args(&daemon.socket, &["--json", "app", "list"])
            .await
            .unwrap();
        let list: alchemy_api::AppList = serde_json::from_str(out.trim_end()).unwrap();
        if !list.apps.is_empty() {
            break list;
        }
        assert!(tokio::time::Instant::now() < deadline, "{out}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(list.apps[0].name.as_str(), "blog");
    assert_eq!(list.apps[0].dir, home.join("blog").to_str().unwrap());

    let out = run_args(&daemon.socket, &["app", "list"]).await.unwrap();
    assert!(
        out.starts_with("NAME  STATUS   SERVICES  FOLDER\nblog  unknown  -"),
        "{out}"
    );
    let out = run_args(&daemon.socket, &["app", "show", "blog", "--json"])
        .await
        .unwrap();
    let app: alchemy_api::apps::App = serde_json::from_str(out.trim_end()).unwrap();
    assert_eq!(app.name.as_str(), "blog");

    let err = run_args(&daemon.socket, &["app", "show", "nope"])
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert_eq!(err.to_string(), "alchemyd: no app is named \"nope\"");
    let err = run_args(
        &daemon.socket,
        &["settings", "set", "app-folders", "relative"],
    )
    .await
    .unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert!(err.to_string().contains("not a full path"), "{err}");

    daemon.stop().await;
}

/// A daemon serving the app `blog`, whose service `web` runs the
/// container `w1`, with logs from a fake source.
struct BlogDaemon {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    logs: alchemyd::logs::FakeLogSource,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), String>>,
}

async fn start_blog() -> BlogDaemon {
    use alchemy_core::engine::{ContainerState, ContainerStatus};
    use alchemyd::apps::{AppsDeps, SystemFs};
    use alchemyd::compose::FakeComposeConfig;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let blog = root.join("apps/blog");
    std::fs::create_dir_all(&blog).unwrap();
    std::fs::write(blog.join("compose.yaml"), "services: {}\n").unwrap();
    let compose = FakeComposeConfig::new();
    compose.project(blog.clone(), "blog", &["web"]);
    let logs = alchemyd::logs::FakeLogSource::new();
    let mut engine = EngineStatus::new(EngineState::Ready);
    engine.endpoint = Some("unix:///fake/docker.sock".into());
    let deps = Deps::new(FixedEngine(engine))
        .with_apps(
            AppsDeps::new(
                std::sync::Arc::new(SystemFs),
                std::sync::Arc::new(compose),
                Some(root.join("apps")),
            )
            .with_scan_interval(Duration::from_millis(50)),
        )
        .with_logs(std::sync::Arc::new(logs.clone()));
    let state = deps.state.clone();
    let socket = root.join("alchemyd.sock");
    let config = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        socket: socket.clone(),
        state_dir: root.join("state"),
    };
    let (stop, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        alchemyd::run_with(config, deps, async {
            let _ = stop_rx.await;
        })
        .await
        .map_err(|e| format!("{e:#}"))
    });
    let mut web = ContainerStatus::new("w1", "blog-web-1", ContainerState::Running);
    web.project = Some("blog".into());
    web.service = Some("web".into());
    state.update_containers(|c| c.replace_all([web]));
    let mut apps = state.watch_apps();
    tokio::time::timeout(
        DEADLINE,
        apps.wait_for(|list| {
            list.apps
                .first()
                .is_some_and(|app| app.services.iter().any(|s| !s.containers.is_empty()))
        }),
    )
    .await
    .unwrap()
    .unwrap();
    BlogDaemon {
        _dir: dir,
        socket,
        logs,
        stop,
        task,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn app_logs_print_the_tail_and_follow_new_lines() {
    use alchemy_api::logs::LogStream;

    let BlogDaemon {
        _dir,
        socket,
        logs,
        stop,
        task,
    } = start_blog().await;
    logs.write(
        "w1",
        LogStream::Stdout,
        "2024-05-06T07:08:09.000000000Z",
        "started",
    );

    let out = run_args(&socket, &["app", "logs", "blog"]).await.unwrap();
    assert_eq!(out, "web | started\n");
    let out = run_args(
        &socket,
        &["--json", "app", "logs", "blog", "web", "--tail", "1"],
    )
    .await
    .unwrap();
    let line: alchemy_api::logs::LogLine = serde_json::from_str(out.trim_end()).unwrap();
    assert_eq!(line.line, "started");
    let err = run_args(&socket, &["app", "logs", "blog", "db"])
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert_eq!(
        err.to_string(),
        "alchemyd: the app blog has no service named \"db\""
    );

    let cli = Cli::try_parse_from([
        "alchemy",
        "--socket",
        socket.to_str().unwrap(),
        "app",
        "logs",
        "blog",
        "-f",
        "--tail",
        "0",
    ])
    .unwrap();
    let mut follow = Watch::start(cli);
    tokio::time::timeout(DEADLINE, async {
        while logs.followers("w1") == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    logs.write(
        "w1",
        LogStream::Stderr,
        "2024-05-06T07:08:10.000000000Z",
        "new line",
    );
    assert_eq!(follow.line().await, "web | new line");

    stop.send(()).unwrap();
    let err = follow.finish().await.unwrap_err();
    assert_eq!(err.exit_code(), 69, "{err}");
    tokio::time::timeout(DEADLINE, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// A daemon whose home holds the app `blog`, which Compose reads as
/// project `blog`, with operations on `runner`.
async fn daemon_with_blog(
    runner: alchemyd::compose::FakeComposeRunner,
) -> (Daemon, tempfile::TempDir) {
    use std::sync::Arc;

    use alchemyd::apps::{AppsDeps, SystemFs};
    use alchemyd::compose::FakeComposeConfig;

    let apps = tempfile::tempdir().unwrap();
    let home = apps.path().canonicalize().unwrap();
    std::fs::create_dir(home.join("blog")).unwrap();
    std::fs::write(
        home.join("blog/compose.yaml"),
        "services:\n  web:\n    image: nginx\n",
    )
    .unwrap();
    let config = FakeComposeConfig::new();
    config.project(home.join("blog"), "blog", &["web"]);
    let daemon = Daemon::start_with(EngineStatus::new(EngineState::Ready), |deps| {
        deps.with_apps(
            AppsDeps::new(Arc::new(SystemFs), Arc::new(config), Some(home))
                .with_scan_interval(Duration::from_millis(20)),
        )
        .with_runner(Arc::new(runner))
    })
    .await;
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let out = run_args(&daemon.socket, &["--json", "app", "show", "blog"]).await;
        if out.is_ok_and(|out| out.contains(r#""project":"blog""#)) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "blog never read");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (daemon, apps)
}

async fn run_args_prompted(
    socket: &Path,
    args: &[&str],
    prompt: &mut FakePrompt,
) -> Result<String, ClientError> {
    let mut command_line = vec!["alchemy", "--socket", socket.to_str().unwrap()];
    command_line.extend_from_slice(args);
    run_prompted(Cli::try_parse_from(command_line).unwrap(), prompt).await
}

#[tokio::test(flavor = "multi_thread")]
async fn lifecycle_actions_run_in_the_daemon_and_follow_to_the_end() {
    use alchemyd::compose::{ComposeAction, FakeComposeRunner, FakeOutcome, FakeScript, Stream};

    let runner = FakeComposeRunner::new()
        .with_script(
            ComposeAction::Up,
            FakeScript::new().line(
                Stream::Stderr,
                r#"{"id":"Container blog-web-1","status":"Done","text":"Started"}"#,
            ),
        )
        .with_script(
            ComposeAction::Pull,
            FakeScript::new().outcome(FakeOutcome::Fail {
                code: 1,
                message: "pull access denied".into(),
            }),
        );
    let (daemon, apps) = daemon_with_blog(runner.clone()).await;
    let socket = daemon.socket.clone();

    let out = run_args(&socket, &["app", "up", "blog"]).await.unwrap();
    assert_eq!(out, "blog is up\n");

    let out = run_args(&socket, &["--json", "app", "up", "blog"])
        .await
        .unwrap();
    let events: Vec<serde_json::Value> = out
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.first().unwrap()["event"], "operation");
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "output" && e["data"]["text"] == "Container blog-web-1 Started"),
        "{out}"
    );
    assert_eq!(events.last().unwrap()["event"], "done");
    assert_eq!(events.last().unwrap()["data"]["state"], "succeeded");

    let err = run_args(&socket, &["app", "pull", "blog"])
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert_eq!(err.kind(), "operation_failed");
    assert_eq!(err.to_string(), "pull blog failed: pull access denied");

    let runs = runner.runs().len();
    let out = run_args(&socket, &["app", "update", "blog", "--dry-run"])
        .await
        .unwrap();
    let blog = apps.path().canonicalize().unwrap().join("blog");
    assert_eq!(
        out,
        format!(
            "would run: docker compose --project-directory {0} --progress json pull\n\
             would run: docker compose --project-directory {0} --progress json up --detach\n",
            blog.display()
        )
    );

    let err = run_args_prompted(&socket, &["app", "down", "blog"], &mut FakePrompt::no_tty())
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 3);
    let mut prompt = FakePrompt::tty([false]);
    let err = run_args_prompted(&socket, &["app", "down", "blog"], &mut prompt)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 3);
    assert_eq!(
        runner.runs().len(),
        runs,
        "a refused down or a dry run runs nothing"
    );
    let out = run_args_prompted(
        &socket,
        &["app", "down", "blog"],
        &mut FakePrompt::tty([true]),
    )
    .await
    .unwrap();
    assert_eq!(out, "blog is down\n");
    assert_eq!(runner.runs().last().unwrap().action(), ComposeAction::Down);

    let err = run_args(&socket, &["app", "start", "nope"])
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert_eq!(err.to_string(), "alchemyd: no app is named \"nope\"");

    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_an_apps_running_operation() {
    use alchemyd::compose::{ComposeAction, FakeComposeRunner, FakeOutcome, FakeScript};

    let runner = FakeComposeRunner::new().with_script(
        ComposeAction::Stop,
        FakeScript::new().outcome(FakeOutcome::UntilCancelled),
    );
    let (daemon, _apps) = daemon_with_blog(runner).await;
    let socket = daemon.socket.clone();

    let cancel = async {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            match run_args(&socket, &["app", "cancel", "blog"]).await {
                Ok(out) => break out,
                Err(err) => assert_eq!(err.kind(), "nothing_to_cancel", "{err}"),
            }
            assert!(tokio::time::Instant::now() < deadline, "stop never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let (stopped, cancelled) = tokio::join!(run_args(&socket, &["app", "stop", "blog"]), cancel);
    assert!(
        cancelled.starts_with("Cancelling stop blog (operation "),
        "{cancelled}"
    );
    let err = stopped.unwrap_err();
    assert_eq!(err.kind(), "operation_failed");
    assert!(
        err.to_string().starts_with("stop blog was cancelled"),
        "{err}"
    );

    let err = run_args(&socket, &["app", "cancel", "blog"])
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert_eq!(
        err.to_string(),
        "nothing to cancel: blog has no operation running"
    );

    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn app_create_validates_then_creates_and_never_replaces() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::Ready)).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("apps");
    let shop = home.join("shop");
    let compose = root.join("compose.yaml");
    let env = root.join("app.env");
    std::fs::write(&compose, "services: {}\n").unwrap();
    std::fs::write(&env, "SECRET=1\n").unwrap();
    let (home_str, compose_str, env_str) = (
        home.to_str().unwrap(),
        compose.to_str().unwrap(),
        env.to_str().unwrap(),
    );
    run_args(&daemon.socket, &["settings", "set", "app-home", home_str])
        .await
        .unwrap();
    let create = ["app", "create", "shop", "--file", compose_str];

    let out = run_args(&daemon.socket, &[&create[..], &["--dry-run"]].concat())
        .await
        .unwrap();
    assert_eq!(
        out,
        format!(
            "would create shop in {}\nCompose project shop, with no services\n",
            shop.display()
        )
    );
    assert!(!home.exists());

    let err = run_args(&daemon.socket, &create).await.unwrap_err();
    assert_eq!(err.exit_code(), 3, "{err}");

    let args = [&create[..], &["--yes", "--env-file", env_str]].concat();
    let out = run_args(&daemon.socket, &args).await.unwrap();
    assert!(
        out.starts_with(&format!("created shop in {}", shop.display())),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(shop.join("compose.yaml")).unwrap(),
        "services: {}\n"
    );
    assert_eq!(
        std::fs::read_to_string(shop.join(".env")).unwrap(),
        "SECRET=1\n"
    );

    let err = run_args(&daemon.socket, &[&create[..], &["--yes"]].concat())
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert!(err.to_string().contains("already exists"), "{err}");

    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn app_create_start_brings_the_new_app_up_or_says_why_not() {
    use std::sync::Arc;

    use alchemyd::apps::{AppsDeps, SystemFs};
    use alchemyd::compose::FakeComposeConfig;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("apps");
    let compose = root.join("compose.yaml");
    std::fs::write(&compose, "services: {}\n").unwrap();
    let config = FakeComposeConfig::new();
    config.project(home.join("web"), "web", &[]);
    let daemon = Daemon::start_with(EngineStatus::new(EngineState::Ready), |deps| {
        deps.with_apps(
            AppsDeps::new(Arc::new(SystemFs), Arc::new(config), Some(home.clone()))
                .with_scan_interval(Duration::from_secs(60)),
        )
    })
    .await;
    let compose_str = compose.to_str().unwrap();

    let start = |name| {
        [
            "app",
            "create",
            name,
            "--file",
            compose_str,
            "--yes",
            "--start",
        ]
    };
    let out = run_args(&daemon.socket, &start("web")).await.unwrap();
    assert_eq!(
        out,
        format!(
            "created web in {}\nCompose project web, with no services\nweb is up\n",
            home.join("web").display()
        )
    );

    let err = run_args(&daemon.socket, &start("blog")).await.unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert!(
        err.to_string().contains("Compose hasn't read blog yet"),
        "{err}"
    );
    assert!(home.join("blog/compose.yaml").exists(), "the save stands");

    daemon.stop().await;
}

/// Send one `GET path` with `headers` to `addr` and read the whole
/// response, lowercasing the head.
async fn tcp_get(addr: &str, path: &str, headers: &str) -> (String, String) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{headers}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut seen = String::new();
    tokio::time::timeout(DEADLINE, stream.read_to_string(&mut seen))
        .await
        .unwrap()
        .unwrap();
    let (head, body) = seen.split_once("\r\n\r\n").unwrap();
    (head.to_ascii_lowercase(), body.to_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn open_prints_a_link_that_signs_a_browser_in_once() {
    let daemon = Daemon::start(ready_desktop()).await;

    let out = run_args(&daemon.socket, &["--json", "open", "--print"])
        .await
        .unwrap();
    let doc: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(doc["opened"], false, "{out}");
    assert_eq!(doc["expires_in"], 60, "{out}");
    let url = doc["url"].as_str().unwrap();
    let (addr, path) = url
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once('/'))
        .unwrap();
    let path = format!("/{path}");
    assert!(path.starts_with("/login?code="), "{url}");

    let (head, _) = tcp_get(addr, "/api/apps", "").await;
    assert!(head.starts_with("http/1.1 401"), "{head}");

    let (head, _) = tcp_get(addr, &path, "").await;
    assert!(head.starts_with("http/1.1 303"), "{head}");
    let cookie = head
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: "))
        .and_then(|value| value.split(';').next())
        .unwrap()
        .to_owned();
    let (head, body) = tcp_get(addr, "/api/session", &format!("Cookie: {cookie}\r\n")).await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert_eq!(body, r#"{"auth":"cookie"}"#);

    let (head, _) = tcp_get(addr, &path, "").await;
    assert!(
        head.starts_with("http/1.1 401"),
        "a link worked twice: {head}"
    );

    let text = run_args(
        &daemon.socket,
        &["open", "--print", "--url-base", "http://localhost:5173"],
    )
    .await
    .unwrap();
    assert!(
        text.starts_with("http://localhost:5173/login?code="),
        "{text}"
    );
    daemon.stop().await;
}
